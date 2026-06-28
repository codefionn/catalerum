//! Cross-pod bridge for the MCP `GET /mcp` server→client push hub (SOUL §26/§16 M7).
//!
//! The `GET /mcp` SSE stream fans **unsolicited** server→client messages out of a
//! per-workspace [`SessionHub`] (`catalerum-mcp`). That hub is a `tokio::broadcast`
//! registry — **process-local**: under the N-replica Deployment (§16 M7) a client
//! only sees pushes produced by the pod that happens to hold its SSE stream. This
//! module removes that gap by relaying the hub over the bus, mirroring the chat
//! [`TokenRelay`](catalerum_bus::TokenRelay) precedent (Valkey pub/sub cross-pod,
//! in-process fallback for single-pod dev).
//!
//! **Placement.** `catalerum-mcp` has no `catalerum-bus` dependency (and Cargo is
//! frozen), so the bridge lives here — the crate that already owns both the bus
//! handle and the process-global hub backing `GET /mcp`.
//!
//! **Shape.** [`publish`] delivers a notification to this pod's local hub *and*
//! relays it over the bus channel [`catalerum_bus::mcp_push_channel`]. Each pod
//! runs one bus-subscriber task ([`install_mcp_push_bridge`]) that forwards
//! received messages into its *own* local hub — so every pod's open GET streams
//! see the push, wherever it was produced.
//!
//! **Loop prevention.** Unlike the token relay, where the producing pod and the
//! WS-holding pod are *distinct roles* (so no pod both publishes and subscribes a
//! turn), here every pod both publishes to and subscribes from the shared channel.
//! Each pod stamps its relayed messages with a per-process **origin nonce**
//! ([`Uuid`]); the subscriber skips any message carrying its own nonce, since that
//! message was already delivered locally at publish time. Without this a pod would
//! re-deliver (and, worse, could re-broadcast) its own push in a loop.
//!
//! **Degradation.** In-process bus (Valkey disabled → single-pod dev): the relay
//! round-trips through an in-memory broadcast and the origin-nonce skip makes it a
//! no-op, so delivery is exactly the local hub — behaviour unchanged. Bus errors
//! are logged at `warn` and never affect local delivery; push is best-effort by
//! nature (the §26 SSE push is latency, not correctness).
//!
//! **Producer honesty.** No production code publishes into the hub yet — the
//! `publish` seam is wired + unit-tested but unproduced (publishing now would be
//! fabricated traffic). This relay is the cross-pod plumbing the §16 M7 audit
//! deferred, ready for the first real producer.

use std::sync::{Arc, LazyLock, OnceLock};
use std::time::Duration;

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;
use tracing::warn;
use uuid::Uuid;

use catalerum_bus::{mcp_push_channel, Bus, RawStream};
use catalerum_core::WorkspaceId;
use catalerum_mcp::{JsonRpcNotification, SessionHub};

/// How long to wait before resubscribing after the bus stream ends or errors, so a
/// flapping Valkey doesn't spin a tight reconnect loop.
const RESUBSCRIBE_DELAY: Duration = Duration::from_secs(3);

/// The wire envelope relayed on the bus push channel: which pod produced it, which
/// workspace it targets, and the already-serialized JSON-RPC message (kept as a
/// string so a receiving pod forwards byte-identical bytes into its local hub, no
/// re-parse/re-serialize — framing stays identical).
#[derive(Serialize, Deserialize)]
struct PushEnvelope {
    /// The originating pod's per-process nonce (see [`McpPushBridge::origin`]).
    origin: Uuid,
    /// The workspace whose `GET /mcp` streams should receive the message.
    workspace: WorkspaceId,
    /// The serialized JSON-RPC notification, ready for `sse_frame`.
    message: String,
}

/// Wires one pod's local [`SessionHub`] to the bus so server→client pushes fan out
/// across pods. Cheap to clone via `Arc`; one instance per process.
pub(crate) struct McpPushBridge {
    /// This pod's local per-workspace fan-out — what the `GET /mcp` streams read.
    hub: SessionHub,
    /// The coordination bus (Valkey pub/sub, or in-process fallback).
    bus: Bus,
    /// This pod's origin nonce, minted once per process; used to skip our own
    /// relayed messages when they come back around the bus (loop prevention).
    origin: Uuid,
}

impl McpPushBridge {
    /// Build a bridge over `hub` and `bus`, minting a fresh origin nonce.
    pub(crate) fn new(hub: SessionHub, bus: Bus) -> Self {
        Self {
            hub,
            bus,
            origin: Uuid::new_v4(),
        }
    }

    /// The producer seam: deliver `notification` to this pod's local streams *and*
    /// relay it over the bus to peer pods. Returns the number of **local**
    /// subscribers reached (the cross-pod relay is fire-and-forget, best-effort).
    pub(crate) fn publish(
        &self,
        workspace: WorkspaceId,
        notification: &JsonRpcNotification,
    ) -> usize {
        let Ok(json) = serde_json::to_string(notification) else {
            return 0;
        };
        let delivered = self.hub.deliver_serialized(workspace, json.clone());
        self.relay_to_bus(workspace, json);
        delivered
    }

    /// Fire-and-forget publish of an envelope to the bus push channel. Best-effort:
    /// a bus error is logged and dropped — local delivery already happened.
    fn relay_to_bus(&self, workspace: WorkspaceId, message: String) {
        let envelope = PushEnvelope {
            origin: self.origin,
            workspace,
            message,
        };
        let Ok(bytes) = serde_json::to_vec(&envelope) else {
            return;
        };
        let bus = self.bus.clone();
        tokio::spawn(async move {
            if let Err(e) = bus.push().publish_raw(mcp_push_channel(), bytes).await {
                warn!(error = %e, "mcp push bridge: bus relay failed (local delivery unaffected)");
            }
        });
    }

    /// Establish the bus subscription (so the pod is provably wired before this
    /// returns — deterministic for callers and tests), then spawn the forward loop.
    /// A transient initial-subscribe error is logged; the loop retries anyway.
    pub(crate) async fn spawn(self: Arc<Self>) -> JoinHandle<()> {
        let initial = match self.bus.push().subscribe_raw(mcp_push_channel()).await {
            Ok(stream) => Some(stream),
            Err(e) => {
                warn!(error = %e, "mcp push bridge: initial subscribe failed; loop will retry");
                None
            }
        };
        tokio::spawn(self.run(initial))
    }

    /// The forward loop: read relayed envelopes off the bus and deliver each into
    /// this pod's local hub (skipping our own). Resubscribes if the stream ends.
    async fn run(self: Arc<Self>, initial: Option<RawStream>) {
        let mut initial = initial;
        loop {
            let mut stream = match initial.take() {
                Some(stream) => stream,
                None => match self.bus.push().subscribe_raw(mcp_push_channel()).await {
                    Ok(stream) => stream,
                    Err(e) => {
                        warn!(error = %e, "mcp push bridge: subscribe failed; retrying");
                        tokio::time::sleep(RESUBSCRIBE_DELAY).await;
                        continue;
                    }
                },
            };
            while let Some(item) = stream.next().await {
                match item {
                    Ok(bytes) => self.forward(&bytes),
                    Err(e) => {
                        warn!(error = %e, "mcp push bridge: bus stream error; resubscribing");
                        break;
                    }
                }
            }
            // Stream ended or errored (Valkey socket dropped) — pause, then resubscribe.
            // With the in-process bus the stream never ends, so this is Valkey-only.
            tokio::time::sleep(RESUBSCRIBE_DELAY).await;
        }
    }

    /// Deliver one relayed envelope into the local hub, unless we produced it.
    fn forward(&self, bytes: &[u8]) {
        let envelope: PushEnvelope = match serde_json::from_slice(bytes) {
            Ok(envelope) => envelope,
            Err(e) => {
                warn!(error = %e, "mcp push bridge: undecodable envelope dropped");
                return;
            }
        };
        if envelope.origin == self.origin {
            // Our own relayed push — already delivered locally at publish time.
            return;
        }
        self.hub
            .deliver_serialized(envelope.workspace, envelope.message);
    }
}

// ---------------------------------------------------------------------------
// Process-global singleton wiring the `GET /mcp` handler to the bridge.
// ---------------------------------------------------------------------------

/// The process-global local hub backing `GET /mcp`. Always available (the GET
/// handler subscribes to it whether or not a bus bridge is installed).
static HUB: LazyLock<SessionHub> = LazyLock::new(SessionHub::new);

/// The installed cross-pod bridge, set once at startup by
/// [`install_mcp_push_bridge`]. `None` until then: publishing is local-only.
static BRIDGE: OnceLock<Arc<McpPushBridge>> = OnceLock::new();

/// The process-global local hub. `GET /mcp` subscribes to this for its SSE stream.
pub(crate) fn hub() -> SessionHub {
    HUB.clone()
}

/// The process-global producer seam: deliver `notification` to this workspace's
/// local `GET /mcp` streams and — once the cross-pod bridge is installed — relay
/// it to peer pods. Returns the number of local subscribers reached.
///
/// Drop-in for `SessionHub::publish` on the old process-local `SESSIONS` hub: a
/// future producer of unsolicited server messages calls this and every open GET
/// stream for the workspace, on *any* pod, receives it.
///
/// **No production code calls this yet** — it is the wired-and-tested seam the §16
/// M7 audit deferred cross-pod fan-out to; publishing fabricated traffic now was
/// explicitly avoided. Exported so the first real producer has an entry point.
pub fn publish_mcp_push(workspace: WorkspaceId, notification: &JsonRpcNotification) -> usize {
    match BRIDGE.get() {
        Some(bridge) => bridge.publish(workspace, notification),
        None => HUB.publish(workspace, notification),
    }
}

/// Install the cross-pod bridge over `bus` and spawn its subscriber task (SOUL §16
/// M7). Call once at startup after the bus is built. Idempotent: a second call is
/// a no-op. Returns the subscriber task handle (or a completed task if already
/// installed).
///
/// With the in-process bus (Valkey disabled) this is a safe no-op cost: the relay
/// round-trips in memory and the origin-nonce skip drops it, so single-pod
/// delivery is unchanged.
pub async fn install_mcp_push_bridge(bus: Bus) -> JoinHandle<()> {
    let bridge = Arc::new(McpPushBridge::new(hub(), bus));
    if BRIDGE.set(bridge.clone()).is_err() {
        // Already installed — don't spawn a second subscriber.
        return tokio::spawn(async {});
    }
    bridge.spawn().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use catalerum_mcp::sse_frame;
    use serde_json::json;
    use tokio::sync::broadcast;

    /// Receive one message with a short timeout; `None` on timeout/close.
    async fn recv(rx: &mut broadcast::Receiver<String>) -> Option<String> {
        match tokio::time::timeout(Duration::from_millis(300), rx.recv()).await {
            Ok(Ok(msg)) => Some(msg),
            _ => None,
        }
    }

    #[tokio::test]
    async fn forwards_across_pods_exactly_once_and_not_back_to_self() {
        // Two bridges (two pods) wired through one in-process bus, each with its
        // own local hub + distinct origin nonce.
        let bus = Bus::in_process();
        let ws = WorkspaceId::new();
        let hub_a = SessionHub::new();
        let hub_b = SessionHub::new();
        let a = Arc::new(McpPushBridge::new(hub_a.clone(), bus.clone()));
        let b = Arc::new(McpPushBridge::new(hub_b.clone(), bus.clone()));

        // Both pods' bus subscribers up before publishing (pub/sub has no backlog).
        let _ta = a.clone().spawn().await;
        let _tb = b.clone().spawn().await;

        // A client's `GET /mcp` SSE subscription on each pod.
        let mut client_a = hub_a.subscribe(ws);
        let mut client_b = hub_b.subscribe(ws);

        let note = JsonRpcNotification::new("notifications/message", json!({"hi": "there"}));
        let expected = serde_json::to_string(&note).unwrap();

        assert_eq!(
            a.publish(ws, &note),
            1,
            "pod A delivers to its own client at once"
        );

        // Pod B sees it exactly once (relayed over the bus, byte-identical framing).
        let got_b = recv(&mut client_b).await.expect("pod B forwards the push");
        assert_eq!(got_b, expected, "forwarded payload is byte-identical");
        assert_eq!(sse_frame(&got_b), sse_frame(&expected), "framing unchanged");
        assert!(recv(&mut client_b).await.is_none(), "exactly once on B");

        // Pod A got only the local copy — NOT a second copy looped back via the bus.
        let got_a = recv(&mut client_a).await.expect("pod A's local delivery");
        assert_eq!(got_a, expected);
        assert!(
            recv(&mut client_a).await.is_none(),
            "no self-loop duplicate on A"
        );
    }

    #[tokio::test]
    async fn single_pod_delivers_once_with_no_self_loop() {
        // Degradation: one pod on the in-process bus behaves like the bare local
        // hub — exactly one delivery, no duplicate from the bus round-trip.
        let bus = Bus::in_process();
        let ws = WorkspaceId::new();
        let hub = SessionHub::new();
        let bridge = Arc::new(McpPushBridge::new(hub.clone(), bus));
        let _task = bridge.clone().spawn().await;
        let mut client = hub.subscribe(ws);

        let note = JsonRpcNotification::new("notifications/message", json!({}));
        assert_eq!(bridge.publish(ws, &note), 1);
        assert!(
            recv(&mut client).await.is_some(),
            "the single local copy arrives"
        );
        assert!(
            recv(&mut client).await.is_none(),
            "no bus self-loop duplicate"
        );
    }

    #[tokio::test]
    async fn publish_with_no_local_subscribers_still_relays_and_returns_zero() {
        // No GET stream open on the producing pod, but a peer pod delivers to its
        // own client — the push is not lost just because the producer has no reader.
        let bus = Bus::in_process();
        let ws = WorkspaceId::new();
        let hub_a = SessionHub::new();
        let hub_b = SessionHub::new();
        let a = Arc::new(McpPushBridge::new(hub_a, bus.clone()));
        let b = Arc::new(McpPushBridge::new(hub_b.clone(), bus.clone()));
        let _ta = a.clone().spawn().await;
        let _tb = b.clone().spawn().await;
        let mut client_b = hub_b.subscribe(ws);

        let note = JsonRpcNotification::new("notifications/message", json!({"n": 1}));
        assert_eq!(
            a.publish(ws, &note),
            0,
            "no local subscriber on the producer"
        );
        assert_eq!(
            recv(&mut client_b).await,
            Some(serde_json::to_string(&note).unwrap()),
            "peer pod still receives the relayed push",
        );
    }
}
