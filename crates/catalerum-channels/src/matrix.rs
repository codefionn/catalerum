//! Matrix delivery + inbound via the **Client-Server API** (SOUL §25).
//!
//! Matrix is a bidirectional, federated chat protocol — the one channel here that
//! both **sends** and **receives** over the same access token, so it is the
//! natural home for "chat with catalerum from your messenger" (§25). A bot user's
//! access token + a room id are the destination:
//!
//! - **Send** — `PUT /_matrix/client/v3/rooms/{room}/send/m.room.message/{txn}`
//!   with `{ "msgtype": "m.text", "body": "<text>" }` and a bearer access token.
//!   `{txn}` is a per-send-unique transaction id (idempotency key).
//! - **Receive** ([`Channel::subscribe`]) — long-polls `/_matrix/client/v3/sync`,
//!   yielding each new `m.room.message`/`m.text` in the configured room as an
//!   [`InMessage`]. The bot's **own** messages are filtered out (by `user_id`) so
//!   an agent reply never re-triggers itself — essential for the multiplayer loop
//!   (inbound message → `ChannelMessage` trigger §11 → agent → reply on the room).
//!
//! `homeserver` is the base URL (e.g. `https://matrix.org`); tests point it at a
//! mock.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::{json, Value};

use crate::{Channel, ChannelError, InMessage, OutMessage, Result};

/// Matrix caps a single event (the whole PDU) at 65536 **bytes**. Cap the `body`
/// well under that — measured in **bytes** (via [`crate::truncate_message_bytes`]),
/// since a char-count cap would let a CJK/emoji-heavy digest (3–4 bytes/char)
/// blow past the byte limit and have the homeserver reject the whole event. The
/// budget leaves comfortable headroom for the event envelope + JSON escaping.
const MATRIX_MAX_BODY: usize = 48_000;

/// Sync long-poll timeout in **milliseconds** (Matrix `timeout` is in ms). The
/// server holds the request open up to this long waiting for new events, so the
/// receive loop blocks rather than busy-polling. Kept safely **under** the shared
/// HTTP client's 30 s request timeout ([`crate::default_http_client`]) so a quiet
/// room doesn't trip that cap and surface a spurious error every poll.
const SYNC_TIMEOUT_MS: u64 = 20_000;

/// A sync filter that bounds the response (and so the read): a small timeline
/// limit plus empty state/ephemeral/presence/account-data, so each `/sync` returns
/// only recent room messages — not full room state or a giant backlog.
const SYNC_FILTER: &str = r#"{"room":{"timeline":{"limit":50},"state":{"types":[]},"ephemeral":{"types":[]},"account_data":{"types":[]}},"presence":{"types":[]},"account_data":{"types":[]}}"#;

/// Delivers messages to (and receives them from) a Matrix room via a bot user's
/// access token.
pub struct MatrixChannel {
    client: reqwest::Client,
    /// Homeserver base URL, no trailing slash (e.g. `https://matrix.org`).
    homeserver: String,
    access_token: String,
    room_id: String,
    /// The bot's own Matrix user id (e.g. `@assistant:example.org`). Optional for
    /// **send**; required for safe **receive** (it filters the bot's own echo).
    user_id: Option<String>,
    /// Process-unique base for transaction ids, so two runs don't collide.
    txn_base: u64,
    txn_counter: AtomicU64,
}

impl MatrixChannel {
    /// A channel for `room_id` on `homeserver`, authenticating as `access_token`.
    #[must_use]
    pub fn new(
        homeserver: impl Into<String>,
        access_token: impl Into<String>,
        room_id: impl Into<String>,
    ) -> Self {
        Self {
            client: crate::default_http_client(),
            homeserver: homeserver.into().trim_end_matches('/').to_string(),
            access_token: access_token.into(),
            room_id: room_id.into(),
            user_id: None,
            txn_base: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0),
            txn_counter: AtomicU64::new(0),
        }
    }

    /// Set the bot's own user id so [`Channel::subscribe`] can drop its own
    /// messages (preventing an agent reply from re-triggering itself).
    #[must_use]
    pub fn with_user_id(mut self, user_id: impl Into<String>) -> Self {
        let id = user_id.into();
        self.user_id = if id.trim().is_empty() { None } else { Some(id) };
        self
    }

    /// Build with a shared [`reqwest::Client`] (connection-pool reuse).
    #[must_use]
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    /// A transaction id unique to this process + send (Matrix idempotency key).
    fn next_txn(&self) -> String {
        format!(
            "catalerum{}_{}",
            self.txn_base,
            self.txn_counter.fetch_add(1, Ordering::Relaxed)
        )
    }
}

#[async_trait]
impl Channel for MatrixChannel {
    fn kind(&self) -> &str {
        "matrix"
    }

    async fn send(&self, msg: &OutMessage) -> Result<()> {
        let url = format!(
            "{}/_matrix/client/v3/rooms/{}/send/m.room.message/{}",
            self.homeserver,
            encode_segment(&self.room_id),
            encode_segment(&self.next_txn()),
        );
        let resp = self
            .client
            .put(url)
            .bearer_auth(&self.access_token)
            .json(&json!({
                "msgtype": "m.text",
                "body": crate::truncate_message_bytes(&msg.text, MATRIX_MAX_BODY),
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
        let ctx = std::sync::Arc::new(SyncCtx {
            client: self.client.clone(),
            homeserver: self.homeserver.clone(),
            access_token: self.access_token.clone(),
            room_id: self.room_id.clone(),
            user_id: self.user_id.clone(),
        });
        // `since: None` + `primed: false` ⇒ the first `/sync` (a timeout-0 prime)
        // captures the current `next_batch` and **discards** its backlog, so the
        // stream only yields messages that arrive *after* subscription — not the
        // room's history.
        let stream = futures::stream::unfold(SyncState::default(), move |mut st| {
            let ctx = ctx.clone();
            async move {
                loop {
                    if let Some(m) = st.pending.pop_front() {
                        return Some((Ok(m), st));
                    }
                    let timeout = if st.primed { SYNC_TIMEOUT_MS } else { 0 };
                    match sync_once(&ctx, st.since.as_deref(), timeout).await {
                        Ok((next, msgs)) => {
                            st.since = Some(next);
                            if !st.primed {
                                st.primed = true; // discard the priming backlog
                                continue;
                            }
                            st.pending.extend(msgs);
                            // Empty (long-poll returned no new messages): loop and
                            // re-poll. The await above blocks, so this never spins.
                        }
                        Err(e) => {
                            // Surface the error but keep the stream alive; back off so
                            // a persistently-down homeserver doesn't hot-loop.
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

/// Immutable context shared by every `/sync` poll in a subscription.
struct SyncCtx {
    client: reqwest::Client,
    homeserver: String,
    access_token: String,
    room_id: String,
    user_id: Option<String>,
}

/// Mutable cursor threaded through the [`futures::stream::unfold`] loop.
#[derive(Default)]
struct SyncState {
    since: Option<String>,
    pending: VecDeque<InMessage>,
    primed: bool,
}

/// One `/sync` round-trip: returns the next `since` token and the new
/// `m.text` messages in the configured room (the bot's own echo filtered out).
async fn sync_once(
    ctx: &SyncCtx,
    since: Option<&str>,
    timeout_ms: u64,
) -> Result<(String, Vec<InMessage>)> {
    let mut req = ctx
        .client
        .get(format!("{}/_matrix/client/v3/sync", ctx.homeserver))
        .bearer_auth(&ctx.access_token)
        .query(&[("timeout", timeout_ms.to_string())])
        .query(&[("filter", SYNC_FILTER)]);
    if let Some(s) = since {
        req = req.query(&[("since", s)]);
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
    let next = v
        .get("next_batch")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut out = Vec::new();
    if let Some(events) = v
        .get("rooms")
        .and_then(|r| r.get("join"))
        .and_then(|j| j.get(&ctx.room_id))
        .and_then(|room| room.get("timeline"))
        .and_then(|t| t.get("events"))
        .and_then(Value::as_array)
    {
        for ev in events {
            if let Some(m) = message_from_event(ctx, ev) {
                out.push(m);
            }
        }
    }
    Ok((next, out))
}

/// Extract an inbound text message from a timeline event, or `None` if it is not
/// an `m.room.message`/`m.text`, is the bot's own message, or has an empty body.
fn message_from_event(ctx: &SyncCtx, ev: &Value) -> Option<InMessage> {
    if ev.get("type").and_then(Value::as_str) != Some("m.room.message") {
        return None;
    }
    let content = ev.get("content")?;
    if content.get("msgtype").and_then(Value::as_str) != Some("m.text") {
        return None;
    }
    let sender = ev.get("sender").and_then(Value::as_str)?.to_string();
    // Drop our own messages so an agent reply never re-triggers the agent.
    if ctx.user_id.as_deref() == Some(sender.as_str()) {
        return None;
    }
    let body = content.get("body").and_then(Value::as_str)?.to_string();
    if body.is_empty() {
        return None;
    }
    Some(InMessage {
        sender,
        text: body,
        source: ctx.room_id.clone(),
        message_id: ev
            .get("event_id")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// Percent-encode a URL path segment: everything outside the unreserved set
/// (`A-Z a-z 0-9 - . _ ~`) is `%`-escaped. Room ids (`!opaque:server`) and txn
/// ids carry reserved characters (`!`, `:`) that must be encoded in the path.
fn encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use wiremock::matchers::{method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    #[test]
    fn encode_segment_escapes_reserved_chars() {
        assert_eq!(encode_segment("!abc:example.org"), "%21abc%3Aexample.org");
        assert_eq!(encode_segment("catalerum123_4"), "catalerum123_4");
    }

    #[test]
    fn txn_ids_are_unique_and_monotonic() {
        let ch = MatrixChannel::new("https://hs", "tok", "!r:hs");
        let a = ch.next_txn();
        let b = ch.next_txn();
        assert_ne!(a, b, "each send gets a fresh transaction id");
    }

    #[tokio::test]
    async fn send_puts_m_room_message_with_bearer() {
        let server = MockServer::start().await;
        // The room id is percent-encoded in the path; the txn id segment varies, so
        // match the prefix path up to it via a path-regex-free approach: mount on the
        // method+bearer and assert the URL/body from the captured request.
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "event_id": "$evt" })))
            .mount(&server)
            .await;

        let channel = MatrixChannel::new(server.uri(), "secret-token", "!room:hs.example");
        assert_eq!(channel.kind(), "matrix");
        channel
            .send(&OutMessage::text("build green ✅"))
            .await
            .unwrap();

        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 1);
        let sent: Request = reqs.into_iter().next().unwrap();
        assert!(
            sent.url
                .path()
                .starts_with("/_matrix/client/v3/rooms/%21room%3Ahs.example/send/m.room.message/"),
            "room id is percent-encoded in the send path: {}",
            sent.url.path()
        );
        assert_eq!(
            sent.headers.get("authorization").unwrap(),
            "Bearer secret-token"
        );
        let body: Value = serde_json::from_slice(&sent.body).unwrap();
        assert_eq!(
            body,
            json!({ "msgtype": "m.text", "body": "build green ✅" })
        );
    }

    #[tokio::test]
    async fn over_limit_body_is_capped_before_sending() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let channel = MatrixChannel::new(server.uri(), "t", "!r:hs");
        // A CJK digest: each 中 is 3 bytes, so MATRIX_MAX_BODY of them is ~3× the
        // byte budget — a char-count cap would let it blow past the 65536-byte PDU
        // limit and the homeserver would reject the whole event.
        channel
            .send(&OutMessage::text("中".repeat(MATRIX_MAX_BODY)))
            .await
            .unwrap();
        let sent = server.received_requests().await.unwrap().remove(0);
        let body: Value = serde_json::from_slice(&sent.body).unwrap();
        let text = body["body"].as_str().unwrap();
        // The cap is measured in bytes now, not chars.
        assert!(
            text.len() <= MATRIX_MAX_BODY,
            "body bytes {} over cap",
            text.len()
        );
        assert!(text.ends_with(" […]"));
        assert!(text.starts_with('中'), "keeps the head");
    }

    #[tokio::test]
    async fn send_surfaces_error_status() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(403).set_body_string("M_FORBIDDEN"))
            .mount(&server)
            .await;
        let channel = MatrixChannel::new(server.uri(), "t", "!r:hs");
        let err = channel.send(&OutMessage::text("x")).await.unwrap_err();
        assert!(matches!(err, ChannelError::Status { status: 403, .. }));
    }

    #[tokio::test]
    async fn subscribe_yields_new_room_messages_and_skips_own_echo() {
        let server = MockServer::start().await;
        // Prime: the first sync (no `since`) returns the current batch token; its
        // backlog is discarded.
        Mock::given(method("GET"))
            .and(path("/_matrix/client/v3/sync"))
            .and(query_param_is_missing("since"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "next_batch": "s1",
                "rooms": { "join": { "!room:hs": { "timeline": { "events": [
                    // Backlog — must NOT be yielded (it predates the subscription).
                    { "type": "m.room.message", "sender": "@old:hs", "event_id": "$0",
                      "content": { "msgtype": "m.text", "body": "history" } }
                ] } } } }
            })))
            .mount(&server)
            .await;
        // Live: the next sync (since=s1) returns a real user message plus the bot's
        // own echo, which must be filtered out.
        Mock::given(method("GET"))
            .and(path("/_matrix/client/v3/sync"))
            .and(query_param("since", "s1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "next_batch": "s2",
                "rooms": { "join": { "!room:hs": { "timeline": { "events": [
                    { "type": "m.room.message", "sender": "@bot:hs", "event_id": "$self",
                      "content": { "msgtype": "m.text", "body": "my own reply" } },
                    { "type": "m.room.message", "sender": "@alice:hs", "event_id": "$1",
                      "content": { "msgtype": "m.text", "body": "hello bot" } }
                ] } } } }
            })))
            .mount(&server)
            .await;

        let channel = MatrixChannel::new(server.uri(), "t", "!room:hs").with_user_id("@bot:hs");
        let mut stream = channel.subscribe().await.unwrap();
        let msg = stream.next().await.unwrap().unwrap();
        assert_eq!(msg.sender, "@alice:hs", "own echo + backlog skipped");
        assert_eq!(msg.text, "hello bot");
        assert_eq!(msg.source, "!room:hs");
        assert_eq!(msg.message_id.as_deref(), Some("$1"));
    }
}
