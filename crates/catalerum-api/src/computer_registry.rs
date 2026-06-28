//! Per-pod registry of **live computer-agent connections** (SOUL §19/§20).
//!
//! When a computer-agent daemon dials in over its authenticated WebSocket
//! (`routes/computer_agents::connect`), the socket handler registers the live
//! connection here, keyed by [`ComputerAgentId`]. The `computer_*` tools then
//! issue [`ComputerOp`]s through [`ComputerRegistry::request`], which correlates
//! each request/response by an opaque id and awaits the reply with a timeout.
//!
//! This registry is **in-memory and pod-local** (SOUL §11): it is not a source of
//! truth, holds no durable state, and an agent connected to a *different* pod is
//! simply "offline" here. The durable truth is the `computer_agents` table;
//! liveness is this map. (Cross-pod forwarding of agent connections — the analogue
//! of the terminal `PodOp` path — is a deliberate v1 deferral.)

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::{mpsc, oneshot, Mutex, RwLock};

use catalerum_bus::Bus;
use catalerum_core::computer::{ComputerCapabilities, ComputerOp, OpResponse, ServerToAgent};
use catalerum_core::{ComputerAgentId, WorkspaceId};
use catalerum_store::Store;

use crate::pod_forward::{forward_computer_op, PodComms, PodOp};

/// Why a [`ComputerRegistry::request`] could not be completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchError {
    /// No live connection for this agent on this pod (never connected, connected
    /// elsewhere, or disconnected).
    Offline,
    /// The agent did not answer within the deadline.
    Timeout,
    /// The connection dropped while the request was in flight.
    Disconnected,
}

impl std::fmt::Display for DispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DispatchError::Offline => write!(f, "the computer agent is not online"),
            DispatchError::Timeout => write!(f, "the computer agent did not respond in time"),
            DispatchError::Disconnected => {
                write!(f, "the computer agent disconnected before responding")
            }
        }
    }
}

impl std::error::Error for DispatchError {}

/// One live agent connection on this pod.
struct LiveConn {
    workspace_id: WorkspaceId,
    name: String,
    capabilities: ComputerCapabilities,
    connected_at: DateTime<Utc>,
    /// Outbound frames to this connection's writer task.
    tx: mpsc::UnboundedSender<ServerToAgent>,
    /// In-flight requests awaiting the agent's response, keyed by correlation id.
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<OpResponse>>>>,
    /// Monotonic request-id source for this connection.
    seq: Arc<AtomicU64>,
    /// Notified when this connection is dropped from the registry (e.g. on revoke),
    /// so the socket handler can tear the physical WebSocket down at once rather
    /// than waiting for the next failed heartbeat.
    close: Arc<tokio::sync::Notify>,
}

/// A snapshot of a live connection for the REST/list surface.
#[derive(Debug, Clone)]
pub struct OnlineAgent {
    pub id: ComputerAgentId,
    pub workspace_id: WorkspaceId,
    pub name: String,
    pub capabilities: ComputerCapabilities,
    pub connected_at: DateTime<Utc>,
}

/// Registry of computer-agent connections. The live map is pod-local; **cross-pod
/// routing** (SOUL §11/§16 M7) is layered on via a Valkey **ownership key**
/// `cat:computer-agent:{id}` → owning pod, announced on connect and refreshed on
/// the heartbeat clock (self-healing: a crashed owner's key lapses). A `request`
/// for an agent this pod doesn't hold looks up the owner and forwards the op over
/// the same sealed `POST /internal/pod` transport the terminal path uses — so a
/// tool call on any pod reaches the agent wherever its socket lives.
pub struct ComputerRegistry {
    conns: RwLock<HashMap<ComputerAgentId, LiveConn>>,
    /// This pod's stable id — the value stored in the ownership key.
    pod_id: String,
    /// Bus handle for the ownership registry (announce/withdraw/lookup). `None` in
    /// unit tests → local-only behaviour (no cross-pod).
    bus: Option<Bus>,
    /// Store for the cross-pod online scan (a workspace's agents, each looked up in
    /// the ownership registry). `None` → local-only.
    store: Option<Store>,
    /// The sealed cross-pod transport, bound once `PodComms` exists (a master key is
    /// configured). `None` → no forwarding (single-pod / no key). A `std` lock: it's
    /// set synchronously at construction and only ever read by cloning the `Arc`
    /// out, so the guard is never held across an await.
    pod_comms: StdRwLock<Option<Arc<PodComms>>>,
    /// HTTP client for the forwarded `POST /internal/pod`.
    http: reqwest::Client,
}

/// Default deadline for a quick agent operation (file/dir/desktop ops). Exec
/// derives its own longer wait from the command's effective timeout instead.
pub const DEFAULT_OP_TIMEOUT: Duration = Duration::from_secs(120);

/// TTL of an ownership announcement — comfortably outlives the 25s WS heartbeat
/// that refreshes it, so a live owner keeps its key while a crashed one lapses.
const OWNER_TTL: Duration = Duration::from_secs(90);

/// The Valkey registry key mapping an agent to the pod holding its live socket.
fn owner_key(id: ComputerAgentId) -> String {
    format!("cat:computer-agent:{id}")
}

/// A placeholder workspace for a forwarded computer op's envelope: the owner routes
/// purely by the agent id in the op payload (see [`forward_computer_op`]).
fn nil_workspace() -> WorkspaceId {
    WorkspaceId::from_uuid(uuid::Uuid::nil())
}

impl ComputerRegistry {
    /// A fresh registry for `pod_id`. `bus`/`store` enable cross-pod ownership +
    /// online discovery (pass `None` for a local-only registry, e.g. in tests);
    /// the sealed forwarding transport is bound later via
    /// [`set_pod_comms`](Self::set_pod_comms) once a master key is available.
    #[must_use]
    pub fn new(pod_id: String, bus: Option<Bus>, store: Option<Store>) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("reqwest client build cannot fail with static options");
        Self {
            conns: RwLock::new(HashMap::new()),
            pod_id,
            bus,
            store,
            pod_comms: StdRwLock::new(None),
            http,
        }
    }

    /// Bind the sealed cross-pod transport once `PodComms` is built (a master key
    /// is configured). Until then, cross-pod `request`/`disconnect` degrade to
    /// "offline" (single-pod deployments never need it). Synchronous so it can be
    /// called from the non-async `AppState::new`.
    pub fn set_pod_comms(&self, comms: Arc<PodComms>) {
        if let Ok(mut slot) = self.pod_comms.write() {
            *slot = Some(comms);
        }
    }

    /// Clone out the bound transport, if any (never holds the lock across an await).
    fn pod_comms(&self) -> Option<Arc<PodComms>> {
        self.pod_comms.read().ok().and_then(|g| g.clone())
    }

    /// Announce (or refresh) this pod as the owner of `id`'s live connection.
    /// Called on connect and on every WS heartbeat so the key stays fresh.
    pub(crate) async fn announce_ownership(&self, id: ComputerAgentId) {
        if let Some(bus) = &self.bus {
            if let Err(e) = bus
                .registry()
                .announce(&owner_key(id), self.pod_id.clone().into_bytes(), OWNER_TTL)
                .await
            {
                tracing::warn!(error = %e, agent = %id, "announcing computer-agent ownership");
            }
        }
    }

    /// Withdraw the ownership announcement (on disconnect) so the agent reads
    /// offline at once rather than waiting for the TTL to lapse.
    async fn withdraw_ownership(&self, id: ComputerAgentId) {
        if let Some(bus) = &self.bus {
            let _ = bus.registry().withdraw(&owner_key(id)).await;
        }
    }

    /// The pod that currently owns `id`'s live socket, per the registry. `None`
    /// when no pod holds it (offline everywhere) or no bus is configured.
    async fn lookup_owner(&self, id: ComputerAgentId) -> Option<String> {
        let bus = self.bus.as_ref()?;
        let raw = bus.registry().lookup(&owner_key(id)).await.ok().flatten()?;
        String::from_utf8(raw).ok()
    }

    /// Register (or replace) the live connection for `id`, returning a
    /// [`Notify`](tokio::sync::Notify) the socket handler awaits to learn when the
    /// connection is dropped from the registry (e.g. by a revoke on another task) so
    /// it can close the socket immediately. Replacing an existing entry notifies the
    /// old one's close handle and drops its pending map (waking any in-flight
    /// requests with [`DispatchError::Disconnected`]).
    pub async fn connect(
        &self,
        id: ComputerAgentId,
        workspace_id: WorkspaceId,
        name: String,
        capabilities: ComputerCapabilities,
        tx: mpsc::UnboundedSender<ServerToAgent>,
    ) -> Arc<tokio::sync::Notify> {
        let close = Arc::new(tokio::sync::Notify::new());
        let conn = LiveConn {
            workspace_id,
            name,
            capabilities,
            connected_at: Utc::now(),
            tx,
            pending: Arc::new(Mutex::new(HashMap::new())),
            seq: Arc::new(AtomicU64::new(0)),
            close: close.clone(),
        };
        if let Some(prev) = self.conns.write().await.insert(id, conn) {
            prev.close.notify_one();
        }
        // Advertise this pod as the owner so other pods can route ops here.
        self.announce_ownership(id).await;
        close
    }

    /// Drop the live connection for `id` (on socket close or revoke). Notifies the
    /// connection's close handle so its socket handler tears the WebSocket down,
    /// withdraws the cross-pod ownership key, and any in-flight requests resolve to
    /// [`DispatchError::Disconnected`] as their oneshots are dropped.
    pub async fn disconnect(&self, id: ComputerAgentId) {
        if let Some(conn) = self.conns.write().await.remove(&id) {
            conn.close.notify_one();
        }
        self.withdraw_ownership(id).await;
    }

    /// Revoke path: drop the connection wherever it lives. Closes it locally if
    /// held here and, if the owner is another pod, forwards a
    /// [`PodOp::ComputerDisconnect`] so a revoke on any pod tears the socket down at
    /// once (the owner's heartbeat revoked-check is the slower safety net).
    pub async fn disconnect_everywhere(&self, id: ComputerAgentId) {
        // Resolve the owner before the local withdraw (disconnect clears the key).
        let owner = self.lookup_owner(id).await;
        self.disconnect(id).await;
        let Some(pod) = owner.filter(|p| p != &self.pod_id) else {
            return;
        };
        let (Some(bus), Some(comms)) = (self.bus.as_ref(), self.pod_comms()) else {
            return;
        };
        let op = PodOp::ComputerDisconnect { agent_id: id };
        if let Err(e) = forward_computer_op(
            &comms,
            bus,
            &self.http,
            &pod,
            nil_workspace(),
            op,
            Duration::from_secs(10),
        )
        .await
        {
            tracing::warn!(error = %e, agent = %id, pod = %pod, "forwarding computer-agent disconnect");
        }
    }

    /// Whether `id` is connected to this pod.
    pub async fn is_online(&self, id: ComputerAgentId) -> bool {
        self.conns.read().await.contains_key(&id)
    }

    /// The live capabilities of `id`, if connected here.
    pub async fn capabilities(&self, id: ComputerAgentId) -> Option<ComputerCapabilities> {
        self.conns
            .read()
            .await
            .get(&id)
            .map(|c| c.capabilities.clone())
    }

    /// Snapshot the agents online in `workspace_id` — **across all pods**. Local
    /// connections come from the live map (with live capabilities); agents held on
    /// other pods are found by listing the workspace's agents and looking each up in
    /// the ownership registry (their capabilities come from the last-persisted
    /// snapshot the owner refreshes on its heartbeat). Falls back to local-only when
    /// no bus/store is configured (single-pod / tests).
    pub async fn online_in_workspace(&self, workspace_id: WorkspaceId) -> Vec<OnlineAgent> {
        let mut out: Vec<OnlineAgent> = self
            .conns
            .read()
            .await
            .iter()
            .filter(|(_, c)| c.workspace_id == workspace_id)
            .map(|(id, c)| OnlineAgent {
                id: *id,
                workspace_id: c.workspace_id,
                name: c.name.clone(),
                capabilities: c.capabilities.clone(),
                connected_at: c.connected_at,
            })
            .collect();

        let (Some(bus), Some(store)) = (&self.bus, &self.store) else {
            return out; // local-only
        };
        let local_ids: HashSet<ComputerAgentId> = out.iter().map(|o| o.id).collect();
        let Ok(agents) = store
            .computer_agents()
            .list_by_workspace(workspace_id)
            .await
        else {
            return out;
        };
        for a in agents {
            if !a.is_active() || local_ids.contains(&a.id) {
                continue;
            }
            let Some(raw) = bus.registry().lookup(&owner_key(a.id)).await.ok().flatten() else {
                continue; // no owner → offline
            };
            let Ok(pod) = String::from_utf8(raw) else {
                continue;
            };
            if pod == self.pod_id {
                continue; // stale self-key without a live conn — treat as offline
            }
            out.push(OnlineAgent {
                id: a.id,
                workspace_id,
                name: a.name,
                capabilities: a.capabilities.unwrap_or_default(),
                connected_at: a.last_seen_at.unwrap_or_else(Utc::now),
            });
        }
        out
    }

    /// Resolve an inbound [`OpResponse`] from `id` to the waiting request, if any.
    pub async fn resolve_response(&self, id: ComputerAgentId, resp: OpResponse) {
        let pending = {
            let conns = self.conns.read().await;
            conns.get(&id).map(|c| c.pending.clone())
        };
        if let Some(pending) = pending {
            if let Some(waiter) = pending.lock().await.remove(&resp.id) {
                let _ = waiter.send(resp);
            }
        }
    }

    /// Send `op` to `id` and await the agent's response (bounded by `timeout`).
    /// The RwLock is never held across the await — the sender/pending handles are
    /// cloned out first — so concurrent requests to different agents don't block
    /// each other.
    pub async fn request(
        &self,
        id: ComputerAgentId,
        op: ComputerOp,
        timeout: Duration,
    ) -> Result<OpResponse, DispatchError> {
        // Fast path: the agent is connected to this pod.
        if self.conns.read().await.contains_key(&id) {
            return self.request_local(id, op, timeout).await;
        }
        // Otherwise route to the pod that owns the live socket.
        self.forward_request(id, op, timeout).await
    }

    /// Run `op` against the **local** connection for `id` only (no cross-pod
    /// fallback) — the owner side of a forwarded request. `Offline` if not held
    /// here (e.g. a stale ownership key after the agent moved pods).
    pub(crate) async fn request_local(
        &self,
        id: ComputerAgentId,
        op: ComputerOp,
        timeout: Duration,
    ) -> Result<OpResponse, DispatchError> {
        let (tx, pending, seq) = {
            let conns = self.conns.read().await;
            let conn = conns.get(&id).ok_or(DispatchError::Offline)?;
            (conn.tx.clone(), conn.pending.clone(), conn.seq.clone())
        };

        let req_id = seq.fetch_add(1, Ordering::Relaxed).to_string();
        let (otx, orx) = oneshot::channel();
        pending.lock().await.insert(req_id.clone(), otx);

        if tx
            .send(ServerToAgent::Request {
                id: req_id.clone(),
                op,
            })
            .is_err()
        {
            pending.lock().await.remove(&req_id);
            return Err(DispatchError::Offline);
        }

        match tokio::time::timeout(timeout, orx).await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(_)) => Err(DispatchError::Disconnected),
            Err(_) => {
                pending.lock().await.remove(&req_id);
                Err(DispatchError::Timeout)
            }
        }
    }

    /// Route `op` to the pod that owns `id`'s live socket, over the sealed
    /// `POST /internal/pod` transport. `Offline` when no owner is known, the owner
    /// is unreachable, or cross-pod forwarding isn't configured (no bus/master key).
    async fn forward_request(
        &self,
        id: ComputerAgentId,
        op: ComputerOp,
        timeout: Duration,
    ) -> Result<OpResponse, DispatchError> {
        let Some(bus) = self.bus.as_ref() else {
            return Err(DispatchError::Offline);
        };
        let Some(comms) = self.pod_comms() else {
            return Err(DispatchError::Offline);
        };
        let owner = match self.lookup_owner(id).await {
            Some(pod) if pod != self.pod_id => pod,
            _ => return Err(DispatchError::Offline),
        };
        let pod_op = PodOp::ComputerRequest {
            agent_id: id,
            computer_op: op,
            timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
        };
        // Give the owner the op's own deadline plus slack for the round trip.
        let call_timeout = timeout + Duration::from_secs(10);
        match forward_computer_op(
            &comms,
            bus,
            &self.http,
            &owner,
            nil_workspace(),
            pod_op,
            call_timeout,
        )
        .await
        {
            Ok(json) => {
                serde_json::from_value::<OpResponse>(json).map_err(|_| DispatchError::Disconnected)
            }
            // The owner pod is unreachable (addr lapsed) or rejected the hop.
            Err(_) => Err(DispatchError::Offline),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use catalerum_core::computer::ComputerCapabilities;

    fn agent_id() -> ComputerAgentId {
        ComputerAgentId::new()
    }

    #[tokio::test]
    async fn offline_request_errors() {
        let reg = ComputerRegistry::new("test-pod".to_string(), None, None);
        let err = reg
            .request(
                agent_id(),
                ComputerOp::Stat {
                    cwd: None,
                    path: "/x".into(),
                },
                Duration::from_millis(50),
            )
            .await
            .unwrap_err();
        assert_eq!(err, DispatchError::Offline);
    }

    #[tokio::test]
    async fn request_response_roundtrip() {
        let reg = Arc::new(ComputerRegistry::new("test-pod".to_string(), None, None));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let id = agent_id();
        let ws = WorkspaceId::new();
        reg.connect(id, ws, "m".into(), ComputerCapabilities::default(), tx)
            .await;
        assert!(reg.is_online(id).await);

        // A fake agent: echo back a success response for whatever it receives.
        let reg2 = reg.clone();
        tokio::spawn(async move {
            if let Some(ServerToAgent::Request { id: req_id, .. }) = rx.recv().await {
                reg2.resolve_response(id, OpResponse::ok(req_id, serde_json::json!({"ok": 1})))
                    .await;
            }
        });

        let resp = reg
            .request(
                id,
                ComputerOp::Stat {
                    cwd: None,
                    path: "/x".into(),
                },
                Duration::from_secs(2),
            )
            .await
            .expect("response");
        assert!(resp.ok);
        assert_eq!(resp.data["ok"], 1);

        reg.disconnect(id).await;
        assert!(!reg.is_online(id).await);
    }

    #[tokio::test]
    async fn disconnect_notifies_the_close_handle() {
        let reg = ComputerRegistry::new("test-pod".to_string(), None, None);
        let (tx, _rx) = mpsc::unbounded_channel();
        let id = agent_id();
        let close = reg
            .connect(
                id,
                WorkspaceId::new(),
                "m".into(),
                ComputerCapabilities::default(),
                tx,
            )
            .await;
        // A revoke elsewhere drops it; the socket handler's `close.notified()` wakes.
        reg.disconnect(id).await;
        // `notify_one` stored a permit, so this returns immediately even though we
        // weren't already awaiting at the moment of the notify.
        tokio::time::timeout(Duration::from_secs(1), close.notified())
            .await
            .expect("close handle was notified on disconnect");
        assert!(!reg.is_online(id).await);
    }

    #[tokio::test]
    async fn timeout_when_agent_silent() {
        let reg = ComputerRegistry::new("test-pod".to_string(), None, None);
        let (tx, _rx) = mpsc::unbounded_channel();
        let id = agent_id();
        reg.connect(
            id,
            WorkspaceId::new(),
            "m".into(),
            ComputerCapabilities::default(),
            tx,
        )
        .await;
        let err = reg
            .request(
                id,
                ComputerOp::Stat {
                    cwd: None,
                    path: "/x".into(),
                },
                Duration::from_millis(60),
            )
            .await
            .unwrap_err();
        assert_eq!(err, DispatchError::Timeout);
    }
}
