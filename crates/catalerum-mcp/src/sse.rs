//! The **streaming** half of the MCP streamable-HTTP transport (SOUL §26/§29).
//!
//! The request/response half is a plain `POST /mcp` answered with one JSON-RPC
//! response ([`crate::server`]). This module carries what streams on top of it —
//! two transport-agnostic pieces (the axum HTTP glue lives in `catalerum-api`):
//!
//! - [`sse_frame`] frames one already-serialized JSON-RPC message as a Server-Sent
//!   Events `data:` event. `serde_json` never emits a raw newline inside a value,
//!   so a message is a single line and maps to exactly one `data:` line plus the
//!   terminating blank line; a defensively multi-line payload still frames validly.
//! - [`SessionHub`] is the per-workspace fan-out for **unsolicited** server→client
//!   messages behind the standalone `GET /mcp` SSE stream. A stream subscribes for
//!   its authenticated principal's workspace; a producer `publish`es a notification
//!   and every open stream for that workspace receives it.
//!
//! **Correlation is by workspace, not by a per-client `Mcp-Session-Id`.** The HTTP
//! MCP surface mints no session id (its `initialize` returns none), so there is no
//! session key to route on; scoping the push channel to the authenticated bearer's
//! workspace is the minimal bookkeeping that makes the GET stream useful and needs
//! no new handshake. Promoting to per-session routing (keyed on an `Mcp-Session-Id`
//! the server assigns) is an additive change if a session handshake later lands.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use futures::Stream;
use tokio::sync::broadcast;

use catalerum_core::id::WorkspaceId;

use crate::protocol::JsonRpcNotification;

/// Messages buffered per workspace channel before a slow subscriber begins to lag.
/// A lagged subscriber drops the missed messages (they are best-effort
/// notifications) rather than ever blocking the publisher.
const CHANNEL_CAPACITY: usize = 64;

/// Frame one already-serialized JSON-RPC message as a single SSE event.
///
/// `payload` is the message serialized to JSON. The result is `data: <payload>\n\n`
/// (one SSE event, terminated by the blank line). A multi-line payload is split so
/// each line gets its own `data:` field, which SSE rejoins with `\n` on the client.
#[must_use]
pub fn sse_frame(payload: &str) -> String {
    let mut out = String::with_capacity(payload.len() + 8);
    for line in payload.split('\n') {
        out.push_str("data: ");
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
    out
}

/// A per-workspace fan-out hub for server→client push over the `GET /mcp` SSE
/// stream. Cheap to clone — every clone shares one channel registry.
#[derive(Clone, Default)]
pub struct SessionHub {
    channels: Arc<RwLock<HashMap<WorkspaceId, broadcast::Sender<String>>>>,
}

impl SessionHub {
    /// An empty hub.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Subscribe to `workspace`'s push channel as a raw broadcast receiver; each
    /// item is one serialized JSON-RPC message ready for [`sse_frame`].
    #[must_use]
    pub fn subscribe(&self, workspace: WorkspaceId) -> broadcast::Receiver<String> {
        self.sender(workspace).subscribe()
    }

    /// Subscribe to `workspace` as a [`Stream`] of serialized JSON-RPC messages,
    /// ready to be SSE-framed and streamed to a client. A lagged subscriber (a slow
    /// client) silently skips the dropped messages; the stream ends only when the
    /// channel is closed.
    pub fn subscribe_stream(
        &self,
        workspace: WorkspaceId,
    ) -> impl Stream<Item = String> + Send + 'static {
        let rx = self.subscribe(workspace);
        futures::stream::unfold(rx, |mut rx| async move {
            loop {
                match rx.recv().await {
                    Ok(msg) => return Some((msg, rx)),
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        })
    }

    /// Push `notification` to every open stream for `workspace`. Returns the number
    /// of subscribers reached — `0` when none are listening, in which case it is a
    /// no-op (a notification with no audience is simply dropped).
    pub fn publish(&self, workspace: WorkspaceId, notification: &JsonRpcNotification) -> usize {
        let Ok(json) = serde_json::to_string(notification) else {
            return 0;
        };
        self.deliver_serialized(workspace, json)
    }

    /// Deliver an **already-serialized** JSON-RPC message to every open stream for
    /// `workspace`, returning the number of subscribers reached (`0` = no-op).
    ///
    /// [`publish`](Self::publish) is this plus the `serde_json::to_string`. It is
    /// exposed so a cross-pod bridge can fan a message *received from the bus*
    /// (already a JSON string) into this pod's local streams **without** re-parsing
    /// and re-serializing it — the bytes forwarded are byte-identical to what the
    /// originating pod published, so [`sse_frame`] frames it identically.
    pub fn deliver_serialized(&self, workspace: WorkspaceId, payload: String) -> usize {
        let channels = self.channels.read().unwrap_or_else(|e| e.into_inner());
        match channels.get(&workspace) {
            Some(tx) => tx.send(payload).unwrap_or(0),
            None => 0,
        }
    }

    /// The broadcast sender for `workspace`, created on first use.
    fn sender(&self, workspace: WorkspaceId) -> broadcast::Sender<String> {
        {
            let channels = self.channels.read().unwrap_or_else(|e| e.into_inner());
            if let Some(tx) = channels.get(&workspace) {
                return tx.clone();
            }
        }
        let mut channels = self.channels.write().unwrap_or_else(|e| e.into_inner());
        channels
            .entry(workspace)
            .or_insert_with(|| broadcast::channel(CHANNEL_CAPACITY).0)
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use serde_json::json;

    #[test]
    fn frames_a_single_line_message_as_one_data_event() {
        assert_eq!(sse_frame(r#"{"id":1}"#), "data: {\"id\":1}\n\n");
    }

    #[test]
    fn frames_each_line_of_a_multiline_payload() {
        assert_eq!(sse_frame("a\nb"), "data: a\ndata: b\n\n");
    }

    #[tokio::test]
    async fn publish_reaches_a_subscribed_workspace_stream() {
        let hub = SessionHub::new();
        let ws = WorkspaceId::new();
        let mut stream = Box::pin(hub.subscribe_stream(ws));
        let note = JsonRpcNotification::progress(json!("tok"), 1.0, Some(1.0), None);
        assert_eq!(hub.publish(ws, &note), 1);
        let msg = stream.next().await.expect("a message arrives");
        let value: serde_json::Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(value["method"], "notifications/progress");
        assert_eq!(value["params"]["progressToken"], "tok");
    }

    #[test]
    fn publish_with_no_subscribers_is_a_noop() {
        let hub = SessionHub::new();
        let note = JsonRpcNotification::new("notifications/message", json!({}));
        assert_eq!(hub.publish(WorkspaceId::new(), &note), 0);
    }

    #[test]
    fn a_push_is_scoped_to_its_own_workspace() {
        let hub = SessionHub::new();
        let listening = WorkspaceId::new();
        let other = WorkspaceId::new();
        let _rx = hub.subscribe(listening);
        // A subscriber exists, but on a *different* workspace than the push target,
        // so the push reaches nobody.
        let note = JsonRpcNotification::new("notifications/message", json!({}));
        assert_eq!(hub.publish(other, &note), 0);
    }
}
