//! The per-workspace sandbox manager (SOUL §20).
//!
//! [`WorkspaceSandboxManager`] is the api-layer owner of the **one long-lived
//! sandbox per workspace** posture: it lazily ensures a workspace's
//! [`WorkspaceSandbox`] on first use, records the durable `workspace_sandboxes`
//! row, tracks live sessions so an in-use sandbox is never reaped, and runs the
//! idle + self-exit reaper. The [`TerminalManager`](crate::terminal::TerminalManager)
//! routes container/kubernetes terminal opens + I/O through it, and the
//! `run_command` tool routes one-shot commands through it, so every command for a
//! workspace executes inside that single sandbox.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use catalerum_bus::Bus;
use catalerum_core::error::{Error, Result};
use catalerum_core::model::{ExecutorKind, SandboxState, WorkspaceSandboxRecord};
use catalerum_core::provider::{ByteStream, CommandResult, CommandSpec, Session, SessionSpec};
use catalerum_core::WorkspaceId;
use catalerum_exec::{SandboxSpec, WorkspaceSandbox};
use catalerum_store::{NewWorkspaceSandbox, Store};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

/// In-memory liveness for a workspace's sandbox: when it was last used and which
/// PTY sessions are currently open in it (an in-use sandbox is never idle-reaped).
struct WsLive {
    last_activity: Instant,
    sessions: HashSet<String>,
}

impl Default for WsLive {
    fn default() -> Self {
        Self {
            last_activity: Instant::now(),
            sessions: HashSet::new(),
        }
    }
}

/// The peer pod that owns `ws`'s **live** sandbox row, when there is one: the
/// row is `Ready` and stamped with a different pod's id. `None` for no row, a
/// non-ready row (stopped/failed sandboxes are re-ensured freely), a legacy
/// unstamped row (adopted, as boot reconcile always did), or our own row.
fn foreign_owner(rec: Option<&WorkspaceSandboxRecord>, self_pod: &str) -> Option<String> {
    let rec = rec?;
    if rec.status != SandboxState::Ready {
        return None;
    }
    match &rec.pod_id {
        Some(owner) if owner != self_pod => Some(owner.clone()),
        _ => None,
    }
}

/// Workspaces that currently hold ≥1 open PTY session. Their sandbox idle-clock
/// must be refreshed each tick so an attached-but-quiet terminal isn't reaped
/// (podman's local reaper already skips these; k8s needs the external heartbeat).
fn workspaces_with_sessions(live: &HashMap<WorkspaceId, WsLive>) -> Vec<WorkspaceId> {
    live.iter()
        .filter(|(_, e)| !e.sessions.is_empty())
        .map(|(ws, _)| *ws)
        .collect()
}

/// Owns the per-workspace sandboxes across the configured backend (SOUL §20).
/// Wrapped in an `Arc` by [`AppState`](crate::state::AppState); one per process.
/// The live container/Pod handles are node-local — only the durable
/// `workspace_sandboxes` row survives a restart (and, for podman, the
/// deterministically-named container, which `ensure` re-adopts).
pub struct WorkspaceSandboxManager {
    sandbox: Arc<dyn WorkspaceSandbox>,
    store: Store,
    backend: ExecutorKind,
    spec: SandboxSpec,
    idle: Duration,
    /// This process's stable pod identity (multi-pod HA, SOUL §16 M7). Stamped on
    /// every sandbox row this manager upserts so boot reconcile reclaims only its
    /// own (+ legacy NULL) rows, never a peer pod's.
    pod_id: String,
    /// Peer-pod discovery (multi-pod HA, SOUL §16 M7): when set, [`ensure`]
    /// refuses to duplicate a node-local sandbox whose durable row is owned by a
    /// still-announced peer pod. Attached once at boot (`set_peers`) alongside
    /// the terminal forwarder; unset = single-pod, no guard (mirrors
    /// `TerminalManager::forwarder`).
    peers: std::sync::OnceLock<Bus>,
    live: RwLock<HashMap<WorkspaceId, WsLive>>,
}

impl WorkspaceSandboxManager {
    /// Build a manager over `sandbox` (the resolved backend), recording rows in
    /// `store`. `backend` is the [`ExecutorKind`] it represents; `spec` the
    /// default sandbox shape; `idle` the no-live-session window before a podman
    /// container is destroyed.
    #[must_use]
    pub fn new(
        sandbox: Arc<dyn WorkspaceSandbox>,
        store: Store,
        backend: ExecutorKind,
        spec: SandboxSpec,
        idle: Duration,
        pod_id: String,
    ) -> Self {
        Self {
            sandbox,
            store,
            backend,
            spec,
            idle,
            pod_id,
            peers: std::sync::OnceLock::new(),
            live: RwLock::new(HashMap::new()),
        }
    }

    /// Attach the bus for peer-pod liveness lookups (multi-pod HA, SOUL §16 M7).
    /// Called once at boot when pod comms are configured; idempotent.
    pub fn set_peers(&self, bus: Bus) {
        let _ = self.peers.set(bus);
    }

    /// The backend this manager drives.
    #[must_use]
    pub fn backend(&self) -> ExecutorKind {
        self.backend
    }

    /// Bump a workspace's in-memory last-activity clock.
    async fn touch(&self, ws: WorkspaceId) {
        self.live.write().await.entry(ws).or_default().last_activity = Instant::now();
    }

    /// Refuse to `ensure` a **node-local** sandbox that a still-live peer pod
    /// owns (multi-pod HA, SOUL §16 M7). Without this, an op landing on the
    /// wrong pod of an N-replica deployment silently mints a second podman
    /// container for the same workspace — a divergent `/work` twin — instead of
    /// failing with something actionable. The k8s backend is exempt: its CR/Pod
    /// is cluster-shared and deterministically named, so every api pod's
    /// `ensure` converges on the same sandbox. A dead owner (not announced on
    /// the bus registry — its container died with it) is taken over, mirroring
    /// the terminal forwarder's "the session died with it" semantics.
    async fn guard_foreign_owner(&self, ws: WorkspaceId) -> Result<()> {
        if self.backend != ExecutorKind::Container {
            return Ok(());
        }
        // No peer discovery attached = single-pod: behave exactly as before.
        let Some(bus) = self.peers.get() else {
            return Ok(());
        };
        let rec = match self.store.workspace_sandboxes().get(ws).await {
            Ok(rec) => rec,
            // Best-effort read, mirroring the best-effort row write below: a DB
            // hiccup must not stop exec.
            Err(e) => {
                tracing::warn!(error = %e, "workspace sandbox ownership read failed; proceeding");
                return Ok(());
            }
        };
        let Some(owner) = foreign_owner(rec.as_ref(), &self.pod_id) else {
            return Ok(());
        };
        let alive = crate::pod_forward::lookup_pod_addr(bus, &owner)
            .await
            .map_err(|e| {
                // Fail closed: proceeding while blind to the owner risks the
                // exact duplication this guard exists to prevent.
                Error::provider(format!(
                    "cannot verify the owning pod (`{owner}`) of this workspace's sandbox: {e}"
                ))
            })?;
        match alive {
            Some(_) => Err(Error::invalid(format!(
                "the workspace sandbox is live on pod `{owner}`; sandbox ops do not \
                 forward across pods yet — retry so the request lands on the owning \
                 pod, or wait for the sandbox to idle out"
            ))),
            None => {
                tracing::info!(
                    workspace = %ws,
                    previous_owner = %owner,
                    "taking over workspace sandbox from a no-longer-announced pod"
                );
                Ok(())
            }
        }
    }

    /// Ensure a workspace's sandbox exists and is running, recording the durable
    /// row. Idempotent; cheap to call before every session/command.
    pub async fn ensure(&self, ws: WorkspaceId) -> Result<()> {
        self.guard_foreign_owner(ws).await?;
        let handle = self.sandbox.ensure(ws, &self.spec).await?;
        // Persist the observed state (best-effort: a DB hiccup must not stop exec).
        let row = NewWorkspaceSandbox {
            backend: handle.backend,
            image: handle.image.clone(),
            status: SandboxState::Ready,
            container_ref: Some(handle.reference.clone()),
            volume_ref: handle.volume.clone(),
            // Own the row (multi-pod HA, SOUL §16 M7) so a peer pod's boot
            // reconcile leaves this workspace's sandbox alone.
            pod_id: Some(self.pod_id.clone()),
        };
        if let Err(e) = self.store.workspace_sandboxes().upsert(ws, &row).await {
            tracing::warn!(error = %e, "failed to record workspace sandbox row");
        }
        self.touch(ws).await;
        Ok(())
    }

    /// Run a one-shot command inside the workspace sandbox.
    pub async fn run(&self, ws: WorkspaceId, cmd: CommandSpec) -> Result<CommandResult> {
        self.ensure(ws).await?;
        let res = self.sandbox.run(ws, cmd).await;
        self.touch(ws).await;
        res
    }

    /// Copy a host file into the workspace sandbox at the absolute in-sandbox
    /// path `dest` (the `stage_object` copy channel for backends whose files
    /// live inside the container).
    pub async fn copy_in(&self, ws: WorkspaceId, src: &std::path::Path, dest: &str) -> Result<()> {
        let res = self.sandbox.copy_in(ws, src, dest).await;
        self.touch(ws).await;
        res
    }

    /// Copy a file out of the workspace sandbox (absolute in-sandbox `src`) to
    /// the host path `dest`, returning its byte size (`store_object`'s channel).
    pub async fn copy_out(
        &self,
        ws: WorkspaceId,
        src: &str,
        dest: &std::path::Path,
    ) -> Result<u64> {
        let res = self.sandbox.copy_out(ws, src, dest).await;
        self.touch(ws).await;
        res
    }

    /// Open an interactive session inside the workspace sandbox. `spec.cwd` is a
    /// path inside the sandbox (e.g. `/work/<name>`). Tracks the session so the
    /// sandbox isn't idle-reaped while it's open.
    pub async fn open_session(&self, ws: WorkspaceId, spec: SessionSpec) -> Result<Session> {
        self.ensure(ws).await?;
        let session = self.sandbox.exec_session(ws, spec).await?;
        let mut live = self.live.write().await;
        let entry = live.entry(ws).or_default();
        entry.sessions.insert(session.id.clone());
        entry.last_activity = Instant::now();
        Ok(session)
    }

    /// Write input to a session's PTY.
    pub async fn session_write(&self, session: &Session, data: Vec<u8>) -> Result<()> {
        self.sandbox.session_write(session, data).await
    }

    /// Drain up to `max_bytes` (0 = all) of a session's buffered output.
    pub async fn session_read(&self, session: &Session, max_bytes: usize) -> Result<Vec<u8>> {
        self.sandbox.session_read(session, max_bytes).await
    }

    /// Subscribe to a session's live output stream.
    pub async fn session_output(&self, session: &Session) -> Result<ByteStream> {
        self.sandbox.session_output(session).await
    }

    /// Resize a session's PTY.
    pub async fn session_resize(&self, session: &Session, cols: u16, rows: u16) -> Result<()> {
        self.sandbox.session_resize(session, cols, rows).await
    }

    /// Close a session (kills the PTY only — the workspace container stays up),
    /// untracking it so the sandbox can later be idle-reaped.
    pub async fn close_session(&self, ws: WorkspaceId, session: &Session) -> Result<()> {
        let res = self.sandbox.close_session(session).await;
        let mut live = self.live.write().await;
        if let Some(entry) = live.get_mut(&ws) {
            entry.sessions.remove(&session.id);
            entry.last_activity = Instant::now();
        }
        res
    }

    /// Reap sessions whose PTY exited on its own (the user ran `exit`); returns
    /// the reaped backend session ids and untracks them. The workspace container
    /// is left running (it may host other sessions / future commands).
    pub async fn reap(&self) -> Result<Vec<String>> {
        let dead = self.sandbox.reap().await?;
        if !dead.is_empty() {
            let dead_set: HashSet<&String> = dead.iter().collect();
            let mut live = self.live.write().await;
            for entry in live.values_mut() {
                entry.sessions.retain(|id| !dead_set.contains(id));
            }
        }
        Ok(dead)
    }

    /// Destroy workspace sandboxes that have no live sessions and have been idle
    /// past the timeout (podman; the k8s operator GCs its own Pods, so this just
    /// stops tracking). Returns how many were torn down. Best-effort + idempotent.
    pub async fn reap_idle(&self) -> Result<usize> {
        let now = Instant::now();
        let idle = self.idle;
        let stale: Vec<WorkspaceId> = {
            let live = self.live.read().await;
            live.iter()
                .filter(|(_, e)| {
                    e.sessions.is_empty() && now.duration_since(e.last_activity) >= idle
                })
                .map(|(ws, _)| *ws)
                .collect()
        };
        let mut count = 0;
        for ws in stale {
            if let Err(e) = self.sandbox.destroy(ws).await {
                tracing::warn!(error = %e, "failed to destroy idle workspace sandbox");
                continue;
            }
            let _ = self
                .store
                .workspace_sandboxes()
                .set_status(ws, SandboxState::Stopped)
                .await;
            self.live.write().await.remove(&ws);
            count += 1;
        }
        Ok(count)
    }

    /// Refresh the idle clock of every workspace with a live PTY session. For
    /// the k8s backend this patches the CR's `status.lastActivity` so the
    /// operator doesn't scale an attached-but-quiet terminal's Pod to 0 out from
    /// under the user; for podman it's a no-op (its local reaper already skips a
    /// sandbox with open sessions). Best-effort — a backend hiccup is logged, not
    /// fatal — and idempotent, so it's safe to run on every reaper tick.
    pub async fn keepalive_active(&self) {
        // Collect the owned ids under a short read lock (dropped at the `;`,
        // before we await the possibly-slow backend keepalives below).
        let guard = self.live.read().await;
        let active = workspaces_with_sessions(&guard);
        drop(guard);
        for ws in active {
            if let Err(e) = self.sandbox.keepalive(ws).await {
                tracing::warn!(error = %e, "workspace sandbox keepalive failed");
            }
        }
    }

    /// Boot reconcile **scoped to this pod** (multi-pod HA, SOUL §16 M7): mark this
    /// pod's persisted sandbox rows (+ legacy NULL rows) `stopped` — their live
    /// handles died with the previous process on this pod; podman re-adopts by name
    /// on the next `ensure`. A peer pod's rows are left alone. Best-effort.
    pub async fn reconcile(&self) {
        match self
            .store
            .workspace_sandboxes()
            .mark_all_stopped_for_pod(&self.pod_id)
            .await
        {
            Ok(n) if n > 0 => tracing::info!(count = n, "reconciled orphaned workspace sandboxes"),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "workspace sandbox reconcile failed"),
        }
    }

    /// Spawn the periodic reaper: self-exited sessions + idle workspace
    /// containers, every 30s. Mirrors the terminal reaper (main.rs).
    #[must_use]
    pub fn spawn(self: Arc<Self>) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            loop {
                tick.tick().await;
                if let Err(e) = self.reap().await {
                    tracing::warn!(error = %e, "workspace sandbox session reap failed");
                }
                if let Err(e) = self.reap_idle().await {
                    tracing::warn!(error = %e, "workspace sandbox idle reap failed");
                }
                // Keep attached-but-quiet sandboxes alive (k8s: refresh the
                // operator's idle clock; podman: no-op).
                self.keepalive_active().await;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live_with(session_ids: &[&str]) -> WsLive {
        WsLive {
            last_activity: Instant::now(),
            sessions: session_ids.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn keepalive_targets_only_workspaces_with_open_sessions() {
        let attached = WorkspaceId::new();
        let quiet = WorkspaceId::new();
        let mut live = HashMap::new();
        live.insert(attached, live_with(&["s1", "s2"]));
        live.insert(quiet, live_with(&[])); // tracked, but no live session
        assert_eq!(
            workspaces_with_sessions(&live),
            vec![attached],
            "only the workspace with an attached session gets a keepalive",
        );
    }

    #[test]
    fn no_open_sessions_means_no_keepalive() {
        let mut live = HashMap::new();
        live.insert(WorkspaceId::new(), live_with(&[]));
        assert!(workspaces_with_sessions(&live).is_empty());
    }

    fn record(status: SandboxState, pod_id: Option<&str>) -> WorkspaceSandboxRecord {
        let now = chrono::Utc::now();
        WorkspaceSandboxRecord {
            workspace_id: WorkspaceId::new(),
            backend: ExecutorKind::Container,
            image: "img".to_string(),
            status,
            container_ref: Some("cat-ws".to_string()),
            volume_ref: None,
            pod_id: pod_id.map(str::to_string),
            last_activity: now,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn foreign_owner_flags_only_a_live_peer_owned_row() {
        // A Ready row stamped with a different pod = the conflict candidate.
        let peer = record(SandboxState::Ready, Some("pod-b"));
        assert_eq!(
            foreign_owner(Some(&peer), "pod-a"),
            Some("pod-b".to_string())
        );
    }

    #[test]
    fn foreign_owner_lets_every_non_conflict_shape_proceed() {
        // No row at all: first ensure for the workspace.
        assert_eq!(foreign_owner(None, "pod-a"), None);
        // Our own row: the normal re-ensure path.
        let ours = record(SandboxState::Ready, Some("pod-a"));
        assert_eq!(foreign_owner(Some(&ours), "pod-a"), None);
        // Legacy unstamped row: adopted, as boot reconcile always did.
        let legacy = record(SandboxState::Ready, None);
        assert_eq!(foreign_owner(Some(&legacy), "pod-a"), None);
        // A non-ready peer row: its container is gone; re-ensure freely.
        for state in [
            SandboxState::Stopped,
            SandboxState::Failed,
            SandboxState::Pending,
        ] {
            let stale = record(state, Some("pod-b"));
            assert_eq!(
                foreign_owner(Some(&stale), "pod-a"),
                None,
                "{state:?} must not block ensure"
            );
        }
    }
}
