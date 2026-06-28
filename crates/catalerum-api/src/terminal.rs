//! The terminal session manager + agent tools (SOUL §20).
//!
//! [`TerminalManager`] is the api-layer owner of interactive terminal sessions:
//! it picks an [`Executor`] backend per session, runs it in a fresh ephemeral
//! temp dir, records the durable `terminal_sessions` row, and flushes that dir to
//! object storage on demand (`persist`). The `*_terminal` / `terminal_*` tools
//! plus the workdir file tools (`read_file` / `create_file` / `edit_file`) are
//! thin, capability-gated clients of it — all on `exec:run` (a protected scope no
//! base role holds, §19). `persist_terminal` is also `exec:run` (single-cap
//! dispatch picks the security-critical gate over the weaker `storage:write`)
//! and only registered when an object store is configured.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use catalerum_core::capability::{Action, Capability, Resource};
use catalerum_core::error::{Error, Result};
use catalerum_core::model::{
    ExecutorKind, GuardFail, TerminalSession, TerminalSessionStatus, ToolGuard,
};
use catalerum_core::provider::{
    strip_workspace_key, workspace_object_key, ByteStream, CommandSpec, Executor, PutMeta, Session,
    SessionSpec, StorageBackend,
};
use catalerum_core::tool::{Tool, ToolContext, ToolRegistry, MODEL_MEDIA_RESULT_FIELD};
use catalerum_core::{ChatMessage, ChatRequest, MediaInput, TerminalSessionId, WorkspaceId};
use catalerum_llm::{run_agent, AgentConfig, OpenRouterClient};
use catalerum_script::{JsLimits, ScriptCodeRunner};
use catalerum_store::{NewTerminalSession, Store};
use futures::StreamExt;
use serde_json::{json, Value as Json};
use tokio::io::AsyncWriteExt;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::config::ExecConfig;
use crate::pod_forward::{b64, from_b64, PodForwarder, PodOp};
use crate::profile_agent::resolve_constrained_profile;
use crate::sandbox::WorkspaceSandboxManager;
use crate::state::{StorageHandle, StorageRegistry};
use crate::subagent_runs::SubagentRunManager;

/// A live session's in-memory handle: which backend runs it + the executor
/// [`Session`] to drive.
struct LiveRef {
    workspace_id: WorkspaceId,
    backend: ExecutorKind,
    session: Session,
    /// Whether this session runs inside the per-workspace sandbox (SOUL §20) — its
    /// I/O is driven by the [`WorkspaceSandboxManager`], not a per-call executor.
    sandboxed: bool,
}

/// How a live session's I/O is driven: a per-call [`Executor`] backend, or the
/// per-workspace sandbox manager.
enum SessionDriver {
    Executor(Arc<dyn Executor>),
    Sandbox(Arc<WorkspaceSandboxManager>),
}

impl SessionDriver {
    async fn write(&self, session: &Session, data: Vec<u8>) -> Result<()> {
        match self {
            SessionDriver::Executor(e) => e.session_write(session, data).await,
            SessionDriver::Sandbox(m) => m.session_write(session, data).await,
        }
    }
    async fn read(&self, session: &Session, max_bytes: usize) -> Result<Vec<u8>> {
        match self {
            SessionDriver::Executor(e) => e.session_read(session, max_bytes).await,
            SessionDriver::Sandbox(m) => m.session_read(session, max_bytes).await,
        }
    }
    async fn output(&self, session: &Session) -> Result<catalerum_core::provider::ByteStream> {
        match self {
            SessionDriver::Executor(e) => e.session_output(session).await,
            SessionDriver::Sandbox(m) => m.session_output(session).await,
        }
    }
    async fn resize(&self, session: &Session, cols: u16, rows: u16) -> Result<()> {
        match self {
            SessionDriver::Executor(e) => e.session_resize(session, cols, rows).await,
            SessionDriver::Sandbox(m) => m.session_resize(session, cols, rows).await,
        }
    }
}

/// Owns interactive terminal sessions across the configured [`Executor`]
/// backends (SOUL §20). Wrapped in an `Arc` by [`AppState`](crate::state::AppState);
/// one per process. PTY/process state is node-local — only the durable
/// `terminal_sessions` row survives a restart.
pub struct TerminalManager {
    backends: HashMap<ExecutorKind, Arc<dyn Executor>>,
    default_backend: ExecutorKind,
    store: Store,
    storage: Option<StorageHandle>,
    ephemeral_root: PathBuf,
    /// Pinned shell (`[exec].shell`); `None` → the backend default.
    shell: Option<String>,
    /// Whether per-session container terminals are configured with a network
    /// namespace that has no egress (`podman.network = "none"`). A terminal
    /// coding subagent is accepted only on an isolated backend: an arbitrary PTY
    /// command cannot otherwise be prevented from publishing before goal review.
    container_network_isolated: bool,
    /// Equivalent network posture for the shared workspace sandbox
    /// (`sandbox_network = "none"|"isolated"`).
    sandbox_network_isolated: bool,
    /// Per-workspace sandbox manager (SOUL §20). When set, container/kubernetes
    /// terminals run *inside* the workspace's single long-lived sandbox.
    sandbox: Option<Arc<WorkspaceSandboxManager>>,
    /// This process's stable pod identity (multi-pod HA, SOUL §16 M7). Stamped on
    /// every session row this manager creates and compared against a row's owner
    /// to reject driving a session whose PTY lives on a different pod.
    pod_id: String,
    /// Cross-pod forwarder (SOUL §16 M7): when set, an op on a session whose
    /// still-active row belongs to a *different* pod is routed to that owner
    /// instead of erroring. Attached once at boot (`set_forwarder`) when pod
    /// comms are configured; unset keeps the precise routing errors.
    forwarder: std::sync::OnceLock<Arc<dyn PodForwarder>>,
    live: RwLock<HashMap<TerminalSessionId, LiveRef>>,
}

/// Where a session op must run: on this pod (a live driver + session handle) or
/// on the peer pod that owns the PTY (forward the op there).
enum Route {
    Local(SessionDriver, Session),
    Remote(String),
}

/// Where a session *file* op must run: against a local host directory, inside
/// the per-workspace sandbox (files live in the container/Pod, reachable only
/// through the backend's copy channel), or on the owning peer pod (whose
/// filesystem holds the workdir).
enum FileRoute {
    Local(PathBuf),
    InSandbox { cwd: String },
    Remote(String),
}

/// The error for file ops that need a host path a container-backed session
/// doesn't have (today only `persist`; the workdir file tools and
/// `stage_object`/`store_object` ride the sandbox copy channel instead).
fn no_host_dir() -> Error {
    Error::invalid("session has no host directory (this backend keeps files inside the container)")
}

impl TerminalManager {
    /// Build a manager over the configured backends (local/sandbox now;
    /// container/k8s as their slices land). `cfg` supplies the default backend +
    /// the persistent/ephemeral dir roots.
    #[must_use]
    pub fn new(
        backends: HashMap<ExecutorKind, Arc<dyn Executor>>,
        store: Store,
        storage: Option<StorageHandle>,
        cfg: &ExecConfig,
        sandbox: Option<Arc<WorkspaceSandboxManager>>,
        pod_id: String,
    ) -> Self {
        let root_or = |configured: &str, fallback: &str| -> PathBuf {
            if configured.trim().is_empty() {
                std::env::temp_dir().join(fallback)
            } else {
                PathBuf::from(configured)
            }
        };
        Self {
            default_backend: cfg.backend_kind(),
            ephemeral_root: root_or(&cfg.ephemeral_root, "catalerum-terminals-ephemeral"),
            shell: (!cfg.shell.trim().is_empty()).then(|| cfg.shell.clone()),
            container_network_isolated: container_network_is_isolated(&cfg.podman.network),
            sandbox_network_isolated: sandbox_network_is_isolated(&cfg.sandbox_network),
            backends,
            store,
            storage,
            sandbox,
            pod_id,
            forwarder: std::sync::OnceLock::new(),
            live: RwLock::new(HashMap::new()),
        }
    }

    /// Attach the cross-pod forwarder (multi-pod HA, SOUL §16 M7). Called once
    /// at boot when pod comms are configured; later calls are no-ops.
    pub fn set_forwarder(&self, forwarder: Arc<dyn PodForwarder>) {
        let _ = self.forwarder.set(forwarder);
    }

    fn forwarder(&self) -> Option<Arc<dyn PodForwarder>> {
        self.forwarder.get().cloned()
    }

    /// Forward one unary op to the owning pod `pod` (SOUL §16 M7). The owner
    /// executes it exactly as if the request had landed there; its error kind
    /// survives the hop.
    pub(crate) async fn forward_remote(
        &self,
        pod: &str,
        workspace_id: WorkspaceId,
        id: TerminalSessionId,
        op: PodOp,
    ) -> Result<Json> {
        let forwarder = self
            .forwarder()
            .ok_or_else(|| Error::invalid("cross-pod forwarding is not configured"))?;
        forwarder.call(pod, workspace_id, id, op).await
    }

    /// The peer pod owning `id`'s PTY, when this op should be forwarded there:
    /// forwarding is configured, no live handle exists here, and the session's
    /// still-active row is stamped with a different pod. `None` falls through to
    /// the local path (which drives a live handle or errors precisely).
    pub(crate) async fn remote_owner(
        &self,
        workspace_id: WorkspaceId,
        id: TerminalSessionId,
    ) -> Option<String> {
        self.forwarder()?;
        {
            let guard = self.live.read().await;
            if guard
                .get(&id)
                .is_some_and(|lr| lr.workspace_id == workspace_id)
            {
                return None;
            }
        }
        match self.store.terminal_sessions().get(workspace_id, id).await {
            Ok(Some(row)) if row.status == TerminalSessionStatus::Active => match row.pod_id {
                Some(pod) if pod != self.pod_id => Some(pod),
                _ => None,
            },
            _ => None,
        }
    }

    /// Whether `backend` should route through the per-workspace sandbox manager.
    fn sandboxed(&self, backend: ExecutorKind) -> bool {
        self.sandbox.is_some()
            && matches!(backend, ExecutorKind::Container | ExecutorKind::Kubernetes)
    }

    /// Whether a terminal backend is actually network-isolated under this
    /// manager's configuration. Local / lightweight-sandbox sessions and the
    /// per-session Kubernetes executor have no enforceable no-egress boundary;
    /// they are deliberately refused for a terminal coding subagent.
    fn backend_network_isolated(&self, backend: ExecutorKind) -> bool {
        if self.sandboxed(backend) {
            return self.sandbox_network_isolated;
        }
        backend == ExecutorKind::Container && self.container_network_isolated
    }

    /// Require an existing session to have an OS-level no-egress boundary before
    /// handing its shell to a subagent. The check is config-derived and therefore
    /// works for a remote-owned row too, assuming the cluster's shared config.
    async fn require_subagent_isolation(
        &self,
        workspace_id: WorkspaceId,
        id: TerminalSessionId,
    ) -> Result<()> {
        let backend = {
            let guard = self.live.read().await;
            guard
                .get(&id)
                .filter(|live| live.workspace_id == workspace_id)
                .map(|live| live.backend)
        };
        let backend = match backend {
            Some(backend) => backend,
            None => self
                .store
                .terminal_sessions()
                .get(workspace_id, id)
                .await
                .map_err(store_err)?
                .filter(|session| session.status == TerminalSessionStatus::Active)
                .map(|session| session.backend)
                .ok_or_else(|| Error::invalid("unknown or closed terminal session"))?,
        };
        if self.backend_network_isolated(backend) {
            return Ok(());
        }
        Err(Error::invalid(format!(
            "terminal_subagent requires a no-egress container session; `{}` is not network-isolated. Open a `container` terminal with podman.network=`none` (or an isolated workspace sandbox), stage the repositories there, then retry",
            backend.as_token()
        )))
    }

    /// Whether any executor backend is configured (gates tool registration).
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        !self.backends.is_empty() || self.sandbox.is_some()
    }

    fn backend(&self, kind: ExecutorKind) -> Result<Arc<dyn Executor>> {
        self.backends.get(&kind).cloned().ok_or_else(|| {
            Error::invalid(format!(
                "terminal backend `{}` is not available",
                kind.as_token()
            ))
        })
    }

    /// Open a terminal. Every terminal is ephemeral: it runs in a fresh temp dir
    /// under the ephemeral root (use `persist_terminal` to keep its files).
    /// `backend_override` selects the executor runtime; `None` uses the default.
    pub async fn open(
        &self,
        workspace_id: WorkspaceId,
        backend_override: Option<ExecutorKind>,
    ) -> Result<TerminalSession> {
        let backend_kind = backend_override.unwrap_or(self.default_backend);
        // A fresh id names both the host temp dir's leaf and the `/work/.ephemeral`
        // subdir inside the per-workspace sandbox. It's a UUID, so it's traversal-safe.
        let sub_name = TerminalSessionId::new().to_string();
        let dir = self
            .ephemeral_root
            .join(workspace_id.to_string())
            .join(&sub_name);

        // Container/kubernetes terminals run *inside* the workspace's single
        // long-lived sandbox (SOUL §20) when it's enabled: each session is an
        // `exec` into the shared `/work` volume at a per-session subdir — files
        // live in the sandbox, so there's no host dir (like the per-session k8s
        // backend). Otherwise a per-call executor backend runs it on a host dir.
        let sandboxed = self.sandboxed(backend_kind);
        let session = if sandboxed {
            let sandbox = self
                .sandbox
                .as_ref()
                .ok_or_else(|| Error::invalid("per-workspace sandbox is not configured"))?;
            let cwd = format!("/work/.ephemeral/{sub_name}");
            sandbox
                .open_session(
                    workspace_id,
                    SessionSpec {
                        cwd: Some(cwd),
                        shell: self.shell.clone(),
                        ..Default::default()
                    },
                )
                .await?
        } else {
            let executor = self.backend(backend_kind)?;
            ensure_dir(&dir).await?;
            let cwd = dir.to_string_lossy().into_owned();
            executor
                .open_session(SessionSpec {
                    cwd: Some(cwd),
                    shell: self.shell.clone(),
                    ..Default::default()
                })
                .await?
        };
        // Trust the backend's reported host dir; do NOT backfill with `cwd`. A k8s
        // or sandboxed session deliberately returns `None` (files live in the
        // Pod/container, unreachable from the api host) — backfilling the empty
        // local scratch dir would make persist/read_file/edit_file silently
        // operate on the wrong, empty dir instead of erroring
        // (`session_host_dir`). local/sandbox/container return `Some(cwd)`
        // themselves, so this is a no-op for them.
        let host_dir = session.host_dir.clone();

        let row = match self
            .store
            .terminal_sessions()
            .create(
                workspace_id,
                &NewTerminalSession {
                    backend: backend_kind,
                    host_dir,
                    sync_prefix: None,
                    // Own the row so a peer pod's boot reconcile leaves it alone and
                    // a request routed to a non-owning pod gets a precise error.
                    pod_id: Some(self.pod_id.clone()),
                },
            )
            .await
        {
            Ok(row) => row,
            Err(e) => {
                // The PTY is already live — tear it down (and drop its temp dir),
                // or a failed row insert leaks a shell with no handle to close it
                // (it never enters `live`, so even the reaper can't find it).
                if sandboxed {
                    if let Some(sandbox) = &self.sandbox {
                        let _ = sandbox.close_session(workspace_id, &session).await;
                    }
                } else if let Ok(executor) = self.backend(backend_kind) {
                    let _ = executor.close_session(&session).await;
                }
                if let Some(dir) = &session.host_dir {
                    let _ = tokio::fs::remove_dir_all(dir).await;
                }
                return Err(store_err(e));
            }
        };

        self.live.write().await.insert(
            row.id,
            LiveRef {
                workspace_id,
                backend: backend_kind,
                session,
                sandboxed,
            },
        );
        Ok(row)
    }

    /// Resolve where an op on a workspace-scoped id must run.
    ///
    /// A terminal's PTY is node-local (multi-pod HA, SOUL §16 M7): only the pod
    /// that opened it holds the in-memory handle. With a live handle here the op
    /// runs locally; without one, a still-`active` row stamped with a **different**
    /// pod routes to that owner ([`Route::Remote`]) when forwarding is configured
    /// — else the precise errors: "owned by another pod (enable session affinity)"
    /// without a forwarder, "no longer live" for this pod's own dead session,
    /// "unknown or closed" otherwise (any store error degrades to that too).
    async fn route(&self, workspace_id: WorkspaceId, id: TerminalSessionId) -> Result<Route> {
        // Copy the fields needed to build the driver under a short read lock, so the
        // (possibly DB-hitting) routing path below runs without the lock held.
        let held = {
            let guard = self.live.read().await;
            guard
                .get(&id)
                .filter(|lr| lr.workspace_id == workspace_id)
                .map(|lr| (lr.sandboxed, lr.backend, lr.session.clone()))
        };
        if let Some((sandboxed, backend, session)) = held {
            let driver = if sandboxed {
                SessionDriver::Sandbox(
                    self.sandbox
                        .clone()
                        .ok_or_else(|| Error::invalid("per-workspace sandbox is not configured"))?,
                )
            } else {
                SessionDriver::Executor(self.backend(backend)?)
            };
            return Ok(Route::Local(driver, session));
        }
        match self.store.terminal_sessions().get(workspace_id, id).await {
            Ok(Some(row)) if row.status == TerminalSessionStatus::Active => match row.pod_id {
                Some(pod) if pod != self.pod_id => {
                    if self.forwarder().is_some() {
                        Ok(Route::Remote(pod))
                    } else {
                        Err(Error::invalid(format!(
                            "terminal session is owned by another pod (`{pod}`); its PTY is \
                             node-local, so this request must be routed to that pod (enable \
                             session affinity)"
                        )))
                    }
                }
                _ => Err(Error::invalid(
                    "terminal session is no longer live on this pod (its process has ended)",
                )),
            },
            _ => Err(Error::invalid("unknown or closed terminal session")),
        }
    }

    /// Write input (a command line — include a trailing newline to run it).
    pub async fn write(
        &self,
        workspace_id: WorkspaceId,
        id: TerminalSessionId,
        data: Vec<u8>,
    ) -> Result<()> {
        match self.route(workspace_id, id).await? {
            Route::Local(driver, session) => driver.write(&session, data).await,
            Route::Remote(pod) => {
                self.forward_remote(
                    &pod,
                    workspace_id,
                    id,
                    PodOp::Write {
                        data_b64: b64(&data),
                    },
                )
                .await?;
                Ok(())
            }
        }
    }

    /// Drain up to `max_bytes` (0 = all) of output produced since the last read.
    pub async fn read(
        &self,
        workspace_id: WorkspaceId,
        id: TerminalSessionId,
        max_bytes: usize,
    ) -> Result<Vec<u8>> {
        match self.route(workspace_id, id).await? {
            Route::Local(driver, session) => driver.read(&session, max_bytes).await,
            Route::Remote(pod) => {
                let result = self
                    .forward_remote(
                        &pod,
                        workspace_id,
                        id,
                        PodOp::Read {
                            max_bytes: max_bytes as u64,
                            wait_secs: 0,
                        },
                    )
                    .await?;
                forwarded_read_bytes(&result)
            }
        }
    }

    /// Like [`read`](Self::read), but **block** up to `wait_secs` for output to
    /// settle before returning — drain-until-quiet (SOUL §20). A PTY's output is
    /// asynchronous, so a read issued right after a `write` may see nothing; this
    /// polls (~100ms) and returns once output has gone quiet for a short streak
    /// after some arrived, or `wait_secs` elapses. `wait_secs == 0` is a plain
    /// [`read`](Self::read). Used by the `terminal_read` automation node so a
    /// one-shot graph read captures a command's full output deterministically.
    pub async fn read_wait(
        &self,
        workspace_id: WorkspaceId,
        id: TerminalSessionId,
        max_bytes: usize,
        wait_secs: u64,
    ) -> Result<Vec<u8>> {
        if wait_secs == 0 {
            return self.read(workspace_id, id, max_bytes).await;
        }
        // A remote-owned session forwards the *whole* waited read as one op — the
        // owner runs the drain-until-quiet loop, so a 10s wait costs one round
        // trip instead of a hundred polls across the pod network.
        if let Some(pod) = self.remote_owner(workspace_id, id).await {
            let result = self
                .forward_remote(
                    &pod,
                    workspace_id,
                    id,
                    PodOp::Read {
                        max_bytes: max_bytes as u64,
                        wait_secs,
                    },
                )
                .await?;
            return forwarded_read_bytes(&result);
        }
        // Output is "settled" after this many consecutive empty polls once some
        // bytes have arrived (≈300ms quiet) — so we return promptly after a command
        // finishes rather than always waiting the whole `wait_secs`.
        const QUIET_POLLS: u32 = 3;
        let poll = std::time::Duration::from_millis(100);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(wait_secs);
        let mut acc: Vec<u8> = Vec::new();
        let mut quiet = 0u32;
        loop {
            // Drain only up to the remaining cap each poll, so bytes past the cap
            // stay buffered for a later read instead of being drained and dropped.
            // `max_bytes == 0` is uncapped (`read(.., 0)` drains everything).
            let remaining = if max_bytes == 0 {
                0
            } else {
                max_bytes.saturating_sub(acc.len())
            };
            if max_bytes != 0 && remaining == 0 {
                break;
            }
            let chunk = self.read(workspace_id, id, remaining).await?;
            if chunk.is_empty() {
                if !acc.is_empty() {
                    quiet += 1;
                    if quiet >= QUIET_POLLS {
                        break;
                    }
                }
            } else {
                acc.extend_from_slice(&chunk);
                quiet = 0;
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(poll).await;
        }
        Ok(acc)
    }

    /// Resize a session's PTY.
    pub async fn resize(
        &self,
        workspace_id: WorkspaceId,
        id: TerminalSessionId,
        cols: u16,
        rows: u16,
    ) -> Result<()> {
        match self.route(workspace_id, id).await? {
            Route::Local(driver, session) => driver.resize(&session, cols, rows).await,
            Route::Remote(pod) => {
                self.forward_remote(&pod, workspace_id, id, PodOp::Resize { cols, rows })
                    .await?;
                Ok(())
            }
        }
    }

    /// Subscribe to a session's live output (for the read-only pane, P3). A
    /// remote-owned session streams the owner's output through the sealed
    /// pod-forward channel, so the web terminal pane works from any pod.
    pub async fn output(
        &self,
        workspace_id: WorkspaceId,
        id: TerminalSessionId,
    ) -> Result<catalerum_core::provider::ByteStream> {
        match self.route(workspace_id, id).await? {
            Route::Local(driver, session) => driver.output(&session).await,
            Route::Remote(pod) => {
                let forwarder = self
                    .forwarder()
                    .ok_or_else(|| Error::invalid("cross-pod forwarding is not configured"))?;
                forwarder.output(&pod, workspace_id, id).await
            }
        }
    }

    /// Close a session: kill its PTY, drop it from the live set, mark the DB row
    /// closed, and (ephemeral) remove the temp dir. Idempotent on the live set.
    pub async fn close(&self, workspace_id: WorkspaceId, id: TerminalSessionId) -> Result<()> {
        let live = {
            let mut guard = self.live.write().await;
            match guard.get(&id) {
                Some(lr) if lr.workspace_id == workspace_id => guard.remove(&id),
                _ => None,
            }
        };
        if let Some(lr) = live {
            if lr.sandboxed {
                // Kills the PTY only — the shared workspace sandbox stays up.
                if let Some(sandbox) = &self.sandbox {
                    let _ = sandbox.close_session(lr.workspace_id, &lr.session).await;
                }
            } else if let Ok(executor) = self.backend(lr.backend) {
                let _ = executor.close_session(&lr.session).await;
            }
            // Every terminal is ephemeral — discard its temp dir (a sandboxed
            // session has no host dir, so this is a no-op there).
            if let Some(dir) = &lr.session.host_dir {
                let _ = tokio::fs::remove_dir_all(dir).await;
            }
        } else if let Some(pod) = self.remote_owner(workspace_id, id).await {
            // The PTY lives on a peer pod — have the owner tear it down (it also
            // marks the row closed). If the owner can't be reached, fall through
            // to the local row close so the session doesn't stay listed as active
            // (the stale-heartbeat sweep would eventually do the same).
            match self
                .forward_remote(&pod, workspace_id, id, PodOp::Close)
                .await
            {
                Ok(_) => return Ok(()),
                Err(e) => tracing::warn!(error = %e, pod = %pod,
                    "cross-pod terminal close failed; closing the row locally"),
            }
        }
        self.store
            .terminal_sessions()
            .set_status(workspace_id, id, TerminalSessionStatus::Closed)
            .await
            .map_err(store_err)?;
        Ok(())
    }

    /// Reap sessions whose process exited on its own (e.g. the user ran `exit`)
    /// without an explicit [`close`](Self::close): each backend tears down the
    /// dead PTY plus any container/Pod, then the live handle is dropped, the DB
    /// row marked closed, and an ephemeral temp dir removed. Returns how many
    /// were reaped. Best-effort + idempotent; run on a periodic clock (main.rs)
    /// so a self-exited shell can't leave a phantom `active` row or a leaked
    /// container/Pod lingering for a whole process lifetime.
    pub async fn reap(&self) -> Result<usize> {
        // Backend session-ids whose process has exited (PTY + container/Pod
        // already torn down by the backend's own `reap`).
        let mut dead: std::collections::HashSet<String> = std::collections::HashSet::new();
        for executor in self.backends.values() {
            match executor.reap().await {
                Ok(ids) => dead.extend(ids),
                Err(e) => tracing::warn!(error = %e, "terminal backend reap failed"),
            }
        }
        // Sandboxed sessions are reaped by the sandbox manager (the workspace
        // container survives a self-exited PTY).
        if let Some(sandbox) = &self.sandbox {
            match sandbox.reap().await {
                Ok(ids) => dead.extend(ids),
                Err(e) => tracing::warn!(error = %e, "workspace sandbox reap failed"),
            }
        }
        if dead.is_empty() {
            return Ok(0);
        }
        // Drop the matching live handles, keeping the fields needed for cleanup.
        let reaped: Vec<(WorkspaceId, TerminalSessionId, Option<String>)> = {
            let mut guard = self.live.write().await;
            let ids: Vec<TerminalSessionId> = guard
                .iter()
                .filter(|(_, lr)| dead.contains(&lr.session.id))
                .map(|(id, _)| *id)
                .collect();
            ids.into_iter()
                .filter_map(|id| {
                    guard
                        .remove(&id)
                        .map(|lr| (lr.workspace_id, id, lr.session.host_dir))
                })
                .collect()
        };
        let count = reaped.len();
        for (ws, id, host_dir) in reaped {
            // Every terminal is ephemeral — discard its temp dir if it had one.
            if let Some(dir) = &host_dir {
                let _ = tokio::fs::remove_dir_all(dir).await;
            }
            let _ = self
                .store
                .terminal_sessions()
                .set_status(ws, id, TerminalSessionStatus::Closed)
                .await;
        }
        Ok(count)
    }

    /// List a workspace's active sessions.
    pub async fn list(&self, workspace_id: WorkspaceId) -> Result<Vec<TerminalSession>> {
        self.store
            .terminal_sessions()
            .list_active(workspace_id)
            .await
            .map_err(store_err)
    }

    /// Whether object storage is configured (gates `persist`).
    #[must_use]
    pub fn has_storage(&self) -> bool {
        self.storage.is_some()
    }

    /// Mirror a directory to object storage under `prefix`, returning the
    /// user-facing keys written. Backs the `persist` (ephemeral flush).
    pub async fn sync_dir(
        &self,
        workspace_id: WorkspaceId,
        dir: &Path,
        prefix: &str,
    ) -> Result<Vec<String>> {
        let storage = self
            .storage
            .as_ref()
            .ok_or_else(|| Error::invalid("object storage is not configured"))?;
        let phys_prefix = workspace_object_key(workspace_id, prefix);
        let physical =
            catalerum_storage::sync_dir_to_backend(dir, storage.backend.as_ref(), &phys_prefix)
                .await?;
        Ok(physical
            .iter()
            .map(|k| strip_workspace_key(workspace_id, k))
            .collect())
    }

    /// Resolve where a session file op must run — against the live in-memory
    /// handle's host directory, or on the owning peer pod (whose filesystem
    /// holds the workdir) when forwarding is configured. Shared by
    /// [`persist`](Self::persist) and the workdir file tools.
    async fn file_route(
        &self,
        workspace_id: WorkspaceId,
        id: TerminalSessionId,
    ) -> Result<FileRoute> {
        // Hot path (untouched): a live in-memory handle on this pod holds the real
        // working dir. A local/sandbox/container session on a host path carries
        // `Some(dir)`; a sandboxed container/k8s session keeps its files inside
        // the container and instead records its in-container workdir (`cwd`) —
        // reachable only through the sandbox copy channel ([`FileRoute::InSandbox`]).
        let live_handle = {
            let guard = self.live.read().await;
            guard
                .get(&id)
                .filter(|lr| lr.workspace_id == workspace_id)
                .map(|lr| {
                    (
                        lr.session.host_dir.clone(),
                        lr.sandboxed,
                        lr.session.cwd.clone(),
                    )
                })
        };
        if let Some((host_dir, sandboxed, cwd)) = live_handle {
            if let Some(dir) = host_dir {
                return Ok(FileRoute::Local(PathBuf::from(dir)));
            }
            if sandboxed {
                if let Some(cwd) = cwd {
                    return Ok(FileRoute::InSandbox { cwd });
                }
            }
            return Err(no_host_dir());
        }

        // No live handle on this pod. Do NOT fall through to the durable row's
        // `host_dir`: it names a path on the pod that opened the session, so a
        // foreign-pod row would aim these file ops at another pod's filesystem and
        // die with a confusing raw I/O error. Consult the row: a row owned by a
        // different pod routes there (forwarded when comms are up, else the precise
        // session-affinity error); otherwise its process is gone from this pod.
        let row = self
            .store
            .terminal_sessions()
            .get(workspace_id, id)
            .await
            .map_err(store_err)?
            .ok_or_else(|| Error::invalid("unknown terminal session"))?;
        match row.pod_id {
            Some(pod) if pod != self.pod_id => {
                if self.forwarder().is_some() {
                    Ok(FileRoute::Remote(pod))
                } else {
                    Err(Error::invalid(format!(
                        "terminal session is owned by another pod (`{pod}`); its files live on \
                         that pod, so this request must be routed to it (enable session affinity)"
                    )))
                }
            }
            _ => Err(Error::invalid(
                "terminal session is no longer live on this pod (its process has ended)",
            )),
        }
    }

    /// Flush a session's working dir (or, with `source_subdir`, just that subdir of
    /// it) to object storage under `prefix` (the on-demand ephemeral persist,
    /// SOUL §20). Returns the user-facing keys.
    ///
    /// Each written file is then **catalogued + notified** exactly like an upload
    /// (`catalogue_and_notify`): it becomes a queryable object (with a durable
    /// ObjectId) and fires a per-file `StorageObject` trigger (created / updated),
    /// so a `git clone` in the terminal → persist here can head a downstream
    /// index/de-index automation without waiting on the periodic storage watch.
    pub async fn persist(
        &self,
        workspace_id: WorkspaceId,
        id: TerminalSessionId,
        prefix: &str,
        source_subdir: Option<&str>,
    ) -> Result<Vec<String>> {
        let host_dir = match self.file_route(workspace_id, id).await? {
            FileRoute::Local(dir) => dir,
            FileRoute::InSandbox { .. } => return Err(no_host_dir()),
            // The owner holds the files and shares the object store — it uploads,
            // catalogues + notifies exactly as if the call had landed there.
            FileRoute::Remote(pod) => {
                let result = self
                    .forward_remote(
                        &pod,
                        workspace_id,
                        id,
                        PodOp::Persist {
                            prefix: prefix.to_string(),
                            source_subdir: source_subdir.map(str::to_string),
                        },
                    )
                    .await?;
                return result
                    .get("keys")
                    .and_then(Json::as_array)
                    .map(|keys| {
                        keys.iter()
                            .filter_map(Json::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .ok_or_else(|| Error::provider("malformed forwarded persist result"));
            }
        };
        // A subdir narrows the copy to one folder of the workdir (e.g. a cloned
        // repo's `docs/`), guarded against `..`/absolute escapes like the file tools.
        let dir = match source_subdir.map(str::trim).filter(|s| !s.is_empty()) {
            Some(sub) => resolve_in_dir(&host_dir, sub)?,
            None => host_dir,
        };
        let keys = self.sync_dir(workspace_id, &dir, prefix).await?;

        // Catalogue + fire a per-file trigger for each written key. Best-effort per
        // file: the bytes are already durable, so a stat/catalogue failure is logged,
        // not surfaced (mirrors `StoreObjectTool::store_one`).
        if let Some(storage) = self.storage.as_ref() {
            for key in &keys {
                let physical = workspace_object_key(workspace_id, key);
                match storage.backend.stat(&physical).await {
                    Ok(mut object) => {
                        object.key = key.clone();
                        crate::routes::storage::catalogue_and_notify(
                            &self.store,
                            workspace_id,
                            &storage.connection,
                            &storage.bucket,
                            &object,
                        )
                        .await;
                    }
                    Err(e) => tracing::warn!(error = %e, key = %key,
                        "failed to stat persisted file for catalogue (bytes stored)"),
                }
            }
        }

        let _ = self
            .store
            .terminal_sessions()
            .set_sync_prefix(workspace_id, id, prefix)
            .await;
        Ok(keys)
    }

    /// A fresh host temp path under the ephemeral root for staging copies
    /// to/from container-backed sessions (the caller removes it when done).
    async fn staging_file(&self) -> Result<PathBuf> {
        let dir = self.ephemeral_root.join(".staging");
        ensure_dir(&dir).await?;
        Ok(dir.join(TerminalSessionId::new().to_string()))
    }

    /// Stream a stored object's bytes from `backend` (at the already-namespaced
    /// `physical_key`) into a session's working directory at `dest_path` — the
    /// inverse of [`persist`](Self::persist) (SOUL §9/§20). The caller resolves the
    /// source store, so a file living on a *different* backend than the terminal's
    /// folder (e.g. an S3 files store vs. a local workdir) lands where the shell can
    /// work on it. `dest_path` is workdir-relative (no absolute / `..`); parent dirs
    /// are created. Works for **every** backend: a session with a host workdir is
    /// written directly (streamed chunk-by-chunk); a container-backed session is
    /// staged through a host temp file and copied in over the sandbox's exec
    /// channel. Returns the byte count written.
    pub async fn stage_object(
        &self,
        workspace_id: WorkspaceId,
        id: TerminalSessionId,
        backend: &dyn StorageBackend,
        physical_key: &str,
        dest_path: &str,
    ) -> Result<u64> {
        match self.file_route(workspace_id, id).await? {
            FileRoute::Local(dir) => {
                let file = resolve_in_dir(&dir, dest_path)?;
                if let Some(parent) = file.parent() {
                    tokio::fs::create_dir_all(parent)
                        .await
                        .map_err(|e| io_err("create parent dir for", &file, e))?;
                }
                download_object_to(backend, physical_key, &file).await
            }
            // The default for container-backed sessions: no host dir, so stage
            // via a temp file + the sandbox copy channel.
            FileRoute::InSandbox { cwd } => {
                let dest = in_container_path(&cwd, dest_path)?;
                let sandbox = self
                    .sandbox
                    .clone()
                    .ok_or_else(|| Error::invalid("per-workspace sandbox is not configured"))?;
                let tmp = self.staging_file().await?;
                let res = async {
                    let total = download_object_to(backend, physical_key, &tmp).await?;
                    sandbox.copy_in(workspace_id, &tmp, &dest).await?;
                    Ok(total)
                }
                .await;
                let _ = tokio::fs::remove_file(&tmp).await;
                res
            }
            FileRoute::Remote(pod) => Err(Error::invalid(format!(
                "terminal session is owned by another pod (`{pod}`); its files live on that \
                 pod, so this request must be routed to it"
            ))),
        }
    }

    /// Stream a file **out of** a session's working directory into `backend` at the
    /// already-namespaced `physical_key` — the inverse of
    /// [`stage_object`](Self::stage_object) (SOUL §9/§20). Where [`persist`](Self::persist)
    /// flushes the whole workdir under a prefix, this hands a shell a way to push a
    /// *single* file it produced back to a files store (which the caller resolves, so
    /// it can land on a different backend than the terminal's folder). `src_path` is
    /// workdir-relative (no absolute / `..`). Streamed chunk-by-chunk so a large file
    /// doesn't buffer whole. Returns the byte count written; errors if `src_path` is
    /// missing or a directory.
    pub async fn store_object(
        &self,
        workspace_id: WorkspaceId,
        id: TerminalSessionId,
        backend: &dyn StorageBackend,
        physical_key: &str,
        src_path: &str,
    ) -> Result<u64> {
        let file = match self.file_route(workspace_id, id).await? {
            FileRoute::Local(dir) => {
                let file = resolve_in_dir(&dir, src_path)?;
                let meta = tokio::fs::metadata(&file)
                    .await
                    .map_err(|e| io_err("stat", &file, e))?;
                if meta.is_dir() {
                    return Err(Error::invalid(format!(
                        "`{src_path}` is a directory, not a file"
                    )));
                }
                file
            }
            // Container-backed session: copy the file out over the sandbox exec
            // channel into a temp file, then upload that (removed below).
            FileRoute::InSandbox { cwd } => {
                let src = in_container_path(&cwd, src_path)?;
                let sandbox = self
                    .sandbox
                    .clone()
                    .ok_or_else(|| Error::invalid("per-workspace sandbox is not configured"))?;
                let tmp = self.staging_file().await?;
                if let Err(e) = sandbox.copy_out(workspace_id, &src, &tmp).await {
                    let _ = tokio::fs::remove_file(&tmp).await;
                    return Err(e);
                }
                tmp
            }
            FileRoute::Remote(pod) => {
                return Err(Error::invalid(format!(
                    "terminal session is owned by another pod (`{pod}`); its files live on \
                     that pod, so this request must be routed to it"
                )))
            }
        };
        let staged = file.starts_with(self.ephemeral_root.join(".staging"));
        let res = async {
            let size = tokio::fs::metadata(&file)
                .await
                .map_err(|e| io_err("stat", &file, e))?
                .len();
            let handle = tokio::fs::File::open(&file)
                .await
                .map_err(|e| io_err("open", &file, e))?;
            // Content type is left unset here; the backend re-guesses it from the key's
            // extension on the re-stat the caller does (local/WebDAV), mirroring an upload.
            let put_meta = PutMeta {
                content_type: None,
                content_length: Some(size),
            };
            backend
                .put(physical_key, file_chunk_stream(handle), put_meta)
                .await?;
            Ok(size)
        }
        .await;
        if staged {
            let _ = tokio::fs::remove_file(&file).await;
        }
        res
    }

    /// Read a text file from a session's working directory (`read_file` tool).
    /// `path` is relative to the workdir (no absolute / `..` segments). An
    /// optional 1-based `offset` line + `limit` line count window a large file.
    /// The returned content is bounded ([`MAX_READ_FILE_BYTES`]); `truncated`
    /// flags either that byte cap or a file past the hard read ceiling. Works
    /// for **every** backend: a host workdir is read directly; a
    /// container-backed session's file is staged out through the sandbox copy
    /// channel first (mirroring `store_object`'s transport).
    pub async fn read_file(
        &self,
        workspace_id: WorkspaceId,
        id: TerminalSessionId,
        path: &str,
        offset: Option<usize>,
        limit: Option<usize>,
    ) -> Result<ReadFile> {
        match self.file_route(workspace_id, id).await? {
            FileRoute::Local(dir) => {
                let file = resolve_in_dir(&dir, path)?;
                read_file_windowed(&file, path, offset, limit).await
            }
            // Files live inside the sandbox: copy the file out to a staging temp
            // file over the exec channel, then window it exactly like a host file
            // (the whole file is staged — the same disk-not-memory bound
            // `store_object` already accepts on this route).
            FileRoute::InSandbox { cwd } => {
                let src = in_container_path(&cwd, path)?;
                let sandbox = self
                    .sandbox
                    .clone()
                    .ok_or_else(|| Error::invalid("per-workspace sandbox is not configured"))?;
                let tmp = self.staging_file().await?;
                let res = async {
                    sandbox.copy_out(workspace_id, &src, &tmp).await?;
                    read_file_windowed(&tmp, path, offset, limit).await
                }
                .await;
                let _ = tokio::fs::remove_file(&tmp).await;
                res
            }
            FileRoute::Remote(pod) => {
                let result = self
                    .forward_remote(
                        &pod,
                        workspace_id,
                        id,
                        PodOp::ReadFile {
                            path: path.to_string(),
                            offset: offset.map(|v| v as u64),
                            limit: limit.map(|v| v as u64),
                        },
                    )
                    .await?;
                forwarded_read_file(&result)
            }
        }
    }

    /// Read one complete image file for native model input. Unlike
    /// [`read_file`](Self::read_file), this preserves bytes and refuses files
    /// over the hard read ceiling rather than sending a corrupt prefix.
    pub async fn read_media_file(
        &self,
        workspace_id: WorkspaceId,
        id: TerminalSessionId,
        path: &str,
    ) -> Result<ReadMediaFile> {
        match self.file_route(workspace_id, id).await? {
            FileRoute::Local(dir) => {
                let file = resolve_in_dir(&dir, path)?;
                read_media_file_bounded(&file, path).await
            }
            FileRoute::InSandbox { cwd } => {
                let src = in_container_path(&cwd, path)?;
                let sandbox = self
                    .sandbox
                    .clone()
                    .ok_or_else(|| Error::invalid("per-workspace sandbox is not configured"))?;
                let tmp = self.staging_file().await?;
                let res = async {
                    sandbox.copy_out(workspace_id, &src, &tmp).await?;
                    read_media_file_bounded(&tmp, path).await
                }
                .await;
                let _ = tokio::fs::remove_file(&tmp).await;
                res
            }
            FileRoute::Remote(pod) => {
                let result = self
                    .forward_remote(
                        &pod,
                        workspace_id,
                        id,
                        PodOp::ReadMediaFile {
                            path: path.to_string(),
                        },
                    )
                    .await?;
                let bytes = result
                    .get("content_b64")
                    .and_then(Json::as_str)
                    .ok_or_else(|| Error::provider("malformed forwarded read_media_file result"))
                    .and_then(from_b64)?;
                Ok(ReadMediaFile {
                    bytes,
                    size: result.get("size").and_then(Json::as_u64).unwrap_or(0),
                })
            }
        }
    }

    /// Create or overwrite a text file in a session's working directory
    /// (`create_file` tool), creating any intermediate directories. Returns the
    /// byte count written and whether an existing file was overwritten. Works
    /// for **every** backend: a host workdir is written directly; a
    /// container-backed session's file rides the sandbox copy channel
    /// (`stage_object`'s transport) via a staging temp file.
    pub async fn write_file(
        &self,
        workspace_id: WorkspaceId,
        id: TerminalSessionId,
        path: &str,
        content: &str,
    ) -> Result<(u64, bool)> {
        match self.file_route(workspace_id, id).await? {
            FileRoute::Local(dir) => {
                let file = resolve_in_dir(&dir, path)?;
                if let Some(parent) = file.parent() {
                    tokio::fs::create_dir_all(parent)
                        .await
                        .map_err(|e| io_err("create parent dir for", &file, e))?;
                }
                let overwrote = tokio::fs::metadata(&file).await.is_ok();
                tokio::fs::write(&file, content.as_bytes())
                    .await
                    .map_err(|e| io_err("write", &file, e))?;
                Ok((content.len() as u64, overwrote))
            }
            FileRoute::InSandbox { cwd } => {
                let dest = in_container_path(&cwd, path)?;
                let sandbox = self
                    .sandbox
                    .clone()
                    .ok_or_else(|| Error::invalid("per-workspace sandbox is not configured"))?;
                // Probe existence first for the created-vs-overwrote flag (the
                // copy-in script always truncates). The path is a positional,
                // never spliced into the script; best-effort — a probe hiccup
                // reports `created`, it doesn't block the write.
                let overwrote = sandbox
                    .run(
                        workspace_id,
                        CommandSpec {
                            argv: vec![
                                "sh".into(),
                                "-c".into(),
                                r#"test -e "$1""#.into(),
                                "sh".into(),
                                dest.clone(),
                            ],
                            timeout_secs: Some(10),
                            ..Default::default()
                        },
                    )
                    .await
                    .map(|r| r.exit_code == 0)
                    .unwrap_or(false);
                let tmp = self.staging_file().await?;
                let res = async {
                    tokio::fs::write(&tmp, content.as_bytes())
                        .await
                        .map_err(|e| io_err("write", &tmp, e))?;
                    // copy_in creates the in-container parent dirs itself.
                    sandbox.copy_in(workspace_id, &tmp, &dest).await?;
                    Ok((content.len() as u64, overwrote))
                }
                .await;
                let _ = tokio::fs::remove_file(&tmp).await;
                res
            }
            FileRoute::Remote(pod) => {
                let result = self
                    .forward_remote(
                        &pod,
                        workspace_id,
                        id,
                        PodOp::WriteFile {
                            path: path.to_string(),
                            content: content.to_string(),
                        },
                    )
                    .await?;
                let bytes = result
                    .get("bytes")
                    .and_then(Json::as_u64)
                    .ok_or_else(|| Error::provider("malformed forwarded write_file result"))?;
                let overwrote = result
                    .get("overwrote")
                    .and_then(Json::as_bool)
                    .unwrap_or(false);
                Ok((bytes, overwrote))
            }
        }
    }

    /// Replace `old` with `new` in a text file in a session's working directory
    /// (`edit_file` tool). `old` must occur exactly once unless `replace_all`.
    /// Returns the number of replacements made. Refuses a non-UTF-8 or
    /// over-the-ceiling file (a partial read would corrupt it on write-back).
    /// Works for **every** backend: a host workdir is edited in place; a
    /// container-backed session's file is staged out over the sandbox copy
    /// channel, edited on the host, and copied back.
    pub async fn edit_file(
        &self,
        workspace_id: WorkspaceId,
        id: TerminalSessionId,
        path: &str,
        old: &str,
        new: &str,
        replace_all: bool,
    ) -> Result<usize> {
        if old.is_empty() {
            return Err(Error::invalid("`old_string` must not be empty"));
        }
        if old == new {
            return Err(Error::invalid(
                "`old_string` and `new_string` are identical",
            ));
        }
        match self.file_route(workspace_id, id).await? {
            FileRoute::Local(dir) => {
                let file = resolve_in_dir(&dir, path)?;
                let content = read_text_for_edit(&file).await?;
                let (updated, count) = apply_edit(&content, old, new, replace_all)?;
                tokio::fs::write(&file, updated.as_bytes())
                    .await
                    .map_err(|e| io_err("write", &file, e))?;
                Ok(count)
            }
            // Files live inside the sandbox: stage the file out, apply the same
            // read/replace contract to the host copy, and copy the result back.
            // Not atomic against a concurrent in-container writer — neither is
            // the local path against a concurrent shell.
            FileRoute::InSandbox { cwd } => {
                let src = in_container_path(&cwd, path)?;
                let sandbox = self
                    .sandbox
                    .clone()
                    .ok_or_else(|| Error::invalid("per-workspace sandbox is not configured"))?;
                let tmp = self.staging_file().await?;
                let res = async {
                    sandbox.copy_out(workspace_id, &src, &tmp).await?;
                    let content = read_text_for_edit(&tmp).await?;
                    let (updated, count) = apply_edit(&content, old, new, replace_all)?;
                    tokio::fs::write(&tmp, updated.as_bytes())
                        .await
                        .map_err(|e| io_err("write", &tmp, e))?;
                    sandbox.copy_in(workspace_id, &tmp, &src).await?;
                    Ok(count)
                }
                .await;
                let _ = tokio::fs::remove_file(&tmp).await;
                res
            }
            FileRoute::Remote(pod) => {
                let result = self
                    .forward_remote(
                        &pod,
                        workspace_id,
                        id,
                        PodOp::EditFile {
                            path: path.to_string(),
                            old: old.to_string(),
                            new: new.to_string(),
                            replace_all,
                        },
                    )
                    .await?;
                result
                    .get("replacements")
                    .and_then(Json::as_u64)
                    .map(|n| n as usize)
                    .ok_or_else(|| Error::provider("malformed forwarded edit_file result"))
            }
        }
    }
}

/// A [`read_file`](TerminalManager::read_file) result: the (windowed, bounded)
/// file text plus metadata for the model.
pub struct ReadFile {
    /// The file's text — line-windowed if `offset`/`limit` were given, then
    /// capped at [`MAX_READ_FILE_BYTES`].
    pub content: String,
    /// Whether `content` was shortened (byte cap, or a file past the ceiling).
    pub truncated: bool,
    /// Total line count of the (bounded) file, regardless of the window.
    pub total_lines: usize,
    /// The file's size on disk, in bytes.
    pub size: u64,
}

/// Complete binary bytes returned for one native image input.
pub struct ReadMediaFile {
    pub bytes: Vec<u8>,
    pub size: u64,
}

/// Decode a forwarded `Read` op result (`{"output_b64": …}`) back to raw bytes.
fn forwarded_read_bytes(result: &Json) -> Result<Vec<u8>> {
    result
        .get("output_b64")
        .and_then(Json::as_str)
        .ok_or_else(|| Error::provider("malformed forwarded read result"))
        .and_then(from_b64)
}

/// Decode a forwarded `ReadFile` op result back into a [`ReadFile`].
fn forwarded_read_file(result: &Json) -> Result<ReadFile> {
    let content = result
        .get("content")
        .and_then(Json::as_str)
        .ok_or_else(|| Error::provider("malformed forwarded read_file result"))?;
    Ok(ReadFile {
        content: content.to_string(),
        truncated: result
            .get("truncated")
            .and_then(Json::as_bool)
            .unwrap_or(false),
        total_lines: result
            .get("total_lines")
            .and_then(Json::as_u64)
            .unwrap_or(0) as usize,
        size: result.get("size").and_then(Json::as_u64).unwrap_or(0),
    })
}

/// The largest `read_file` content returned inline (256 KiB) — bounded so a big
/// file can't blow up a tool result / the model's context.
const MAX_READ_FILE_BYTES: usize = 256 * 1024;

/// Hard ceiling on bytes the file tools read from disk (8 MiB): a multi-GB file
/// can't OOM the worker. `read_file` reports such a file `truncated`; `edit_file`
/// refuses it (editing a partial read would corrupt the file on write-back).
const HARD_READ_BYTES: u64 = 8 * 1024 * 1024;

/// Resolve a workdir-relative `path` against a session's host `dir`, rejecting
/// absolute paths and `.`/`..` traversal so a file tool can never escape the
/// session's working directory (the relative-key contract storage enforces, §18).
fn resolve_in_dir(dir: &Path, rel: &str) -> Result<PathBuf> {
    use std::path::Component;
    let rel = rel.trim();
    if rel.is_empty() {
        return Err(Error::invalid("`path` must not be empty"));
    }
    let candidate = Path::new(rel);
    if !candidate
        .components()
        .all(|c| matches!(c, Component::Normal(_)))
    {
        return Err(Error::invalid(
            "`path` must be relative to the terminal's working directory \
             (no '.', '..', or absolute segments)",
        ));
    }
    Ok(dir.join(candidate))
}

/// Join a workdir-relative `path` onto a container-backed session's in-container
/// working dir — [`resolve_in_dir`]'s counterpart for paths that live inside the
/// sandbox. Same containment contract (no absolute / `.` / `..` segments); the
/// result is handed to the sandbox copy channel as a shell *positional*, never
/// spliced into a script, so containment is the only concern (not quoting).
fn in_container_path(cwd: &str, path: &str) -> Result<String> {
    use std::path::Component;
    let rel = path.trim();
    if rel.is_empty() {
        return Err(Error::invalid("`path` must not be empty"));
    }
    if !Path::new(rel)
        .components()
        .all(|c| matches!(c, Component::Normal(_)))
    {
        return Err(Error::invalid(
            "`path` must be relative to the terminal's working directory \
             (no '.', '..', or absolute segments)",
        ));
    }
    if rel.contains('\n') {
        return Err(Error::invalid("`path` must not contain newlines"));
    }
    Ok(format!("{}/{rel}", cwd.trim_end_matches('/')))
}

/// Stream a stored object's bytes into a local file, returning the byte count —
/// the shared download half of `stage_object` (direct into a host workdir, or
/// into the staging temp file for container-backed sessions).
async fn download_object_to(
    backend: &dyn StorageBackend,
    physical_key: &str,
    file: &Path,
) -> Result<u64> {
    let mut stream = backend.get(physical_key).await?;
    let mut out = tokio::fs::File::create(file)
        .await
        .map_err(|e| io_err("create", file, e))?;
    let mut total: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        total += chunk.len() as u64;
        out.write_all(&chunk)
            .await
            .map_err(|e| io_err("write", file, e))?;
    }
    out.flush().await.map_err(|e| io_err("flush", file, e))?;
    Ok(total)
}

/// Stream an open file's bytes in fixed-size chunks as a [`ByteStream`], so pushing
/// a large workdir file into a storage backend (`store_object`) bounds memory to one
/// chunk rather than the whole file — the write-side mirror of the backends' own
/// chunked `get`. An I/O error mid-read surfaces as a stream error.
fn file_chunk_stream(file: tokio::fs::File) -> ByteStream {
    use tokio::io::AsyncReadExt;
    /// 64 KiB — the same chunk size the storage backends' `get` uses: large enough
    /// to amortise syscalls, small enough to keep the per-read allocation cheap.
    const CHUNK: usize = 64 * 1024;
    futures::stream::try_unfold(file, |mut file| async move {
        let mut buf = vec![0u8; CHUNK];
        let n = file
            .read(&mut buf)
            .await
            .map_err(|e| Error::other(format!("reading file for upload: {e}")))?;
        if n == 0 {
            Ok(None) // EOF
        } else {
            buf.truncate(n);
            Ok(Some((buf, file)))
        }
    })
    .boxed()
}

/// The shared on-disk body of [`TerminalManager::read_file`]: stat + bounded
/// read + line-window + byte-cap one file. `file` is either the resolved host
/// workdir path or a staging temp file a sandbox copy was staged out to;
/// `path` is the caller's workdir-relative label (for error messages).
async fn read_file_windowed(
    file: &Path,
    path: &str,
    offset: Option<usize>,
    limit: Option<usize>,
) -> Result<ReadFile> {
    let meta = tokio::fs::metadata(file)
        .await
        .map_err(|e| io_err("stat", file, e))?;
    if meta.is_dir() {
        return Err(Error::invalid(format!(
            "`{path}` is a directory, not a file"
        )));
    }
    // Bounded read so a multi-GB file can't OOM the worker; a file past the
    // ceiling is reported truncated (and the on-disk size still reflects it).
    let (bytes, over_cap) = read_file_bounded(file, HARD_READ_BYTES).await?;
    let full = text_file_content(bytes, path, over_cap)?;
    let total_lines = full.lines().count();
    // Window by line if asked (1-based offset); else the whole (bounded) file.
    let windowed: String = match (offset, limit) {
        (None, None) => full,
        (off, lim) => {
            let start = off.unwrap_or(1).saturating_sub(1);
            let mut it = full.lines().skip(start);
            let selected: Vec<&str> = match lim {
                Some(n) => it.by_ref().take(n).collect(),
                None => it.by_ref().collect(),
            };
            selected.join("\n")
        }
    };
    let (content, byte_capped) = cap_text(&windowed, MAX_READ_FILE_BYTES);
    Ok(ReadFile {
        content,
        truncated: over_cap || byte_capped,
        total_lines,
        size: meta.len(),
    })
}

async fn read_media_file_bounded(file: &Path, path: &str) -> Result<ReadMediaFile> {
    let meta = tokio::fs::metadata(file)
        .await
        .map_err(|e| io_err("stat", file, e))?;
    if meta.is_dir() {
        return Err(Error::invalid(format!(
            "`{path}` is a directory, not a file"
        )));
    }
    let (bytes, over_cap) = read_file_bounded(file, HARD_READ_BYTES).await?;
    if over_cap {
        return Err(Error::invalid(format!(
            "`{path}` is too large for native model ingestion (maximum {HARD_READ_BYTES} bytes)"
        )));
    }
    Ok(ReadMediaFile {
        bytes,
        size: meta.len(),
    })
}

fn text_file_content(bytes: Vec<u8>, path: &str, truncated: bool) -> Result<String> {
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) if truncated && error.utf8_error().error_len().is_none() => {
            let valid_up_to = error.utf8_error().valid_up_to();
            let mut bytes = error.into_bytes();
            bytes.truncate(valid_up_to);
            String::from_utf8(bytes).expect("prefix ending at valid_up_to is valid UTF-8")
        }
        Err(_) => {
            return Err(Error::invalid(format!(
                "`{path}` is binary or is not valid UTF-8; read_file reads text files only"
            )))
        }
    };
    if text
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(Error::invalid(format!(
            "`{path}` contains binary control bytes; read_file reads text files only"
        )));
    }
    Ok(text)
}

/// Load a file's text for [`TerminalManager::edit_file`], refusing an
/// over-the-ceiling or non-UTF-8 file (a partial or lossy read would corrupt
/// it on write-back).
async fn read_text_for_edit(file: &Path) -> Result<String> {
    let (bytes, over_cap) = read_file_bounded(file, HARD_READ_BYTES).await?;
    if over_cap {
        return Err(Error::invalid(
            "file is too large to edit safely; edit it from the terminal instead",
        ));
    }
    String::from_utf8(bytes)
        .map_err(|_| Error::invalid("file is not valid UTF-8; edit_file only edits text files"))
}

/// Apply the `edit_file` replacement contract to `content`: `old` must occur
/// exactly once unless `replace_all`. Returns the updated text and how many
/// replacements were made.
fn apply_edit(content: &str, old: &str, new: &str, replace_all: bool) -> Result<(String, usize)> {
    let count = content.matches(old).count();
    if count == 0 {
        return Err(Error::invalid("`old_string` was not found in the file"));
    }
    if count > 1 && !replace_all {
        return Err(Error::invalid(format!(
            "`old_string` is not unique ({count} matches); add surrounding context to \
             target one occurrence, or pass replace_all=true"
        )));
    }
    let updated = if replace_all {
        content.replace(old, new)
    } else {
        content.replacen(old, new, 1)
    };
    Ok((updated, if replace_all { count } else { 1 }))
}

/// Read up to `cap` bytes of `file`; the bool is whether the file exceeded `cap`
/// (so the returned bytes are a prefix). Bounds memory on a huge file.
async fn read_file_bounded(file: &Path, cap: u64) -> Result<(Vec<u8>, bool)> {
    use tokio::io::AsyncReadExt;
    let f = tokio::fs::File::open(file)
        .await
        .map_err(|e| io_err("open", file, e))?;
    // Read one byte past the cap so we can detect (and flag) an oversized file.
    let mut limited = f.take(cap + 1);
    let mut buf = Vec::new();
    limited
        .read_to_end(&mut buf)
        .await
        .map_err(|e| io_err("read", file, e))?;
    let over = buf.len() as u64 > cap;
    if over {
        buf.truncate(cap as usize);
    }
    Ok((buf, over))
}

/// Cap `text` to `max` bytes on a UTF-8 char boundary; returns the (possibly
/// shortened) text and whether it was truncated.
fn cap_text(text: &str, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text.to_string(), false);
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

/// Map a file io error to a tool [`Error`], keeping "not found" precise (the
/// host path is already exposed to the agent via `open_terminal`'s `host_dir`).
fn io_err(op: &str, file: &Path, e: std::io::Error) -> Error {
    if e.kind() == std::io::ErrorKind::NotFound {
        return Error::invalid(format!("no such file: {}", file.display()));
    }
    Error::provider(format!("failed to {op} {}: {e}", file.display()))
}

fn store_err(e: catalerum_store::StoreError) -> Error {
    Error::provider(format!("terminal store error: {e}"))
}

/// Podman/Docker's explicit no-egress token. A network literally named
/// `isolated` is not necessarily isolated, so only the runtime-defined `none`
/// value satisfies the per-session container boundary.
fn container_network_is_isolated(value: &str) -> bool {
    value.trim().eq_ignore_ascii_case("none")
}

/// Workspace-sandbox tokens mapped to an isolated network namespace / K8s
/// NetworkPolicy by the sandbox backends.
fn sandbox_network_is_isolated(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "none" | "isolated"
    )
}

async fn ensure_dir(dir: &Path) -> Result<()> {
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| Error::provider(format!("create terminal dir {}: {e}", dir.display())))
}

// ---------------------------------------------------------------------------
// Tool-call glue
// ---------------------------------------------------------------------------

/// The workspace a tool call is scoped to (every authenticated run carries one).
fn workspace(ctx: &ToolContext) -> Result<WorkspaceId> {
    ctx.workspace_id
        .ok_or_else(|| Error::invalid("tool call has no workspace context"))
}

/// The `exec:run` capability shared by the terminal tools (deny-by-default, §19).
fn exec_cap() -> Option<Capability> {
    Some(Capability::new(Action::Run, Resource::domain("exec")))
}

/// Parse the required `session_id` argument into a typed id.
fn session_id_arg(args: &Json) -> Result<TerminalSessionId> {
    args.get("session_id")
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::invalid("`session_id` is required"))?
        .parse()
        .map_err(|_| Error::invalid("`session_id` must be a uuid"))
}

/// Parse the required `path` argument (a workdir-relative file path). The
/// relative/no-traversal contract is enforced later in [`resolve_in_dir`].
fn path_arg(args: &Json) -> Result<&str> {
    args.get("path")
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::invalid("`path` is required"))
}

/// Common commands worth advertising when a terminal opens. Availability is
/// probed inside the actual interactive session: backend images differ, so a
/// compile-time or API-host list would be misleading for container/k8s sessions.
const TERMINAL_COMMAND_CANDIDATES: [&str; 16] = [
    "python3", "python", "go", "node", "npm", "rustc", "cargo", "git", "sqlite3", "curl", "wget",
    "jq", "make", "gcc", "g++", "java",
];

const COMMAND_PROBE_START: &str = "__CATALERUM_COMMANDS__";
const COMMAND_PROBE_END: &str = "__CATALERUM_COMMANDS_END__";
const MAX_OPEN_STAGE_FILES: usize = 256;

/// Parse the final marked block from a PTY probe. A terminal echoes its input,
/// including the marker literals, so the *last* start marker is the command's
/// output rather than the echoed source line. Only the fixed candidate list is
/// accepted; shell prompts and startup messages can never become advertisements.
fn parse_available_commands(output: &str) -> Vec<&'static str> {
    let Some((_, after_start)) = output.rsplit_once(COMMAND_PROBE_START) else {
        return Vec::new();
    };
    let Some((body, _)) = after_start.split_once(COMMAND_PROBE_END) else {
        return Vec::new();
    };
    TERMINAL_COMMAND_CANDIDATES
        .iter()
        .copied()
        .filter(|candidate| body.lines().any(|line| line.trim() == *candidate))
        .collect()
}

/// Check a small, useful command set from inside the newly opened session.
/// Failure is deliberately non-fatal: opening a usable shell is more important
/// than capability advertising, and the response reports that no commands were
/// discovered rather than claiming unverified availability.
async fn probe_available_commands(
    manager: &TerminalManager,
    workspace_id: WorkspaceId,
    session_id: TerminalSessionId,
) -> (Vec<&'static str>, bool) {
    let candidates = TERMINAL_COMMAND_CANDIDATES.join(" ");
    let script = format!(
        "printf '{COMMAND_PROBE_START}\\n'; for cmd in {candidates}; do command -v \"$cmd\" >/dev/null 2>&1 && printf '%s\\n' \"$cmd\"; done; printf '{COMMAND_PROBE_END}\\n'\n"
    );
    if let Err(error) = manager
        .write(workspace_id, session_id, script.into_bytes())
        .await
    {
        tracing::debug!(%error, %session_id, "terminal command probe write failed");
        return (Vec::new(), false);
    }
    match manager
        .read_wait(workspace_id, session_id, 64 * 1024, 2)
        .await
    {
        Ok(output) => {
            let text = String::from_utf8_lossy(&output);
            let complete = text
                .rsplit_once(COMMAND_PROBE_START)
                .is_some_and(|(_, tail)| tail.contains(COMMAND_PROBE_END));
            (parse_available_commands(&text), complete)
        }
        Err(error) => {
            tracing::debug!(%error, %session_id, "terminal command probe read failed");
            (Vec::new(), false)
        }
    }
}

/// `open_terminal` — stand up an interactive terminal session.
struct OpenTerminalTool {
    manager: Arc<TerminalManager>,
    storage: StorageRegistry,
    store: Store,
}

#[async_trait]
impl Tool for OpenTerminalTool {
    fn name(&self) -> &str {
        "open_terminal"
    }
    fn required_capability(&self) -> Option<Capability> {
        exec_cap()
    }
    fn description(&self) -> &str {
        "Stand up an interactive, ephemeral terminal you can drive over multiple \
         steps. It runs in a throwaway temp dir — use persist_terminal to keep its \
         files. Returns a `session_id`, a live-checked list of common commands \
         installed in that session, and related tool names. Drive it with \
         terminal_write + terminal_read; use stage_object to bring stored files \
         into it and store_object/persist_terminal to keep outputs. A separate \
         run_command call does not operate in this terminal's private workdir. \
         You may also pass `files` and/or `directories` to stage several stored \
         objects directly while opening the session. After preparing repositories \
         in a no-egress container terminal, `terminal_subagent` can run a \
         profile-style Boa-guarded coding worker over this session."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "backend": {
                    "type": "string",
                    "enum": ["local", "sandbox", "container", "kubernetes"],
                    "description": "Override the runtime for the terminal."
                },
                "files": {
                    "type": "array",
                    "maxItems": MAX_OPEN_STAGE_FILES,
                    "description": "Stored files to stage into the new terminal before returning. Each entry uses the same key/store/dest_path fields as stage_object.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "key": { "type": "string", "description": "Store-relative object key." },
                            "store": { "type": "string", "description": "Source store; omitted uses your default files store." },
                            "dest_path": { "type": "string", "description": "Destination relative to the terminal workdir; omitted uses the source filename." }
                        },
                        "required": ["key"]
                    }
                },
                "directories": {
                    "type": "array",
                    "description": "Stored key prefixes/directories to stage recursively. At most 256 files total may be staged across files and directories.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "prefix": { "type": "string", "description": "Non-empty store-relative directory prefix, e.g. `finance/`." },
                            "store": { "type": "string", "description": "Source store; omitted uses your default files store." },
                            "dest_dir": { "type": "string", "description": "Destination directory relative to the terminal workdir; omitted preserves the source directory's final name." }
                        },
                        "required": ["prefix"]
                    }
                }
            }
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let backend = match args.get("backend").and_then(Json::as_str) {
            Some(s) if !s.trim().is_empty() => Some(
                ExecutorKind::parse_token(s)
                    .ok_or_else(|| Error::invalid(format!("unknown backend `{s}`")))?,
            ),
            _ => None,
        };
        let session = self.manager.open(ws, backend).await?;
        let stager = StageObjectTool {
            manager: self.manager.clone(),
            storage: self.storage.clone(),
            store: self.store.clone(),
        };
        let staging = stager.stage_open_requests(&args, ws, session.id, ctx).await;
        let (available_commands, command_probe_succeeded) =
            probe_available_commands(self.manager.as_ref(), ws, session.id).await;
        Ok(json!({
            "session_id": session.id,
            "backend": session.backend.as_token(),
            "host_dir": session.host_dir,
            "staging": staging,
            "available_commands": available_commands,
            "command_probe": {
                "checked": command_probe_succeeded,
                "candidate_count": TERMINAL_COMMAND_CANDIDATES.len(),
                "note": "Live-checked inside this terminal; the list is useful but not exhaustive."
            },
            "related_tools": [
                "terminal_write", "terminal_read", "stage_object", "read_file",
                "create_file", "edit_file", "store_object", "persist_terminal",
                "close_terminal", "terminal_subagent"
            ],
            "advertise_tools": [
                "terminal_write", "terminal_read", "stage_object", "read_file",
                "create_file", "edit_file", "store_object", "persist_terminal",
                "close_terminal", "terminal_subagent"
            ],
            "usage_note": "Use terminal_write and terminal_read with this session_id for commands that must see this terminal's files. Do not use run_command for staged/session files."
        }))
    }
}

/// `terminal_write` — send input (a command line) to a session.
struct TerminalWriteTool {
    manager: Arc<TerminalManager>,
}

#[async_trait]
impl Tool for TerminalWriteTool {
    fn name(&self) -> &str {
        "terminal_write"
    }
    fn required_capability(&self) -> Option<Capability> {
        exec_cap()
    }
    fn description(&self) -> &str {
        "Write input to a terminal session's stdin. Include a trailing newline \
         in `data` to run a command. Read the result with terminal_read. Use this \
         (not run_command) for commands that must access files created by \
         create_file or copied into this session by stage_object."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "description": "From open_terminal." },
                "data": { "type": "string", "description": "Bytes to write (add \\n to execute)." }
            },
            "required": ["session_id", "data"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let id = session_id_arg(&args)?;
        let data = args
            .get("data")
            .and_then(Json::as_str)
            .ok_or_else(|| Error::invalid("`data` is required"))?;
        self.manager.write(ws, id, data.as_bytes().to_vec()).await?;
        Ok(json!({ "ok": true }))
    }
}

/// `terminal_read` — drain a session's accumulated output.
struct TerminalReadTool {
    manager: Arc<TerminalManager>,
}

#[async_trait]
impl Tool for TerminalReadTool {
    fn name(&self) -> &str {
        "terminal_read"
    }
    fn required_capability(&self) -> Option<Capability> {
        exec_cap()
    }
    fn description(&self) -> &str {
        "Read output a terminal session has produced since your last read. \
         Returns `output` (decoded text). Poll after terminal_write, or set \
         `wait_secs` to block until a command's output settles before returning."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "description": "From open_terminal." },
                "wait_secs": { "type": "integer", "description": "Block up to N seconds, draining until output goes quiet, before returning (optional). Use after running a command so a single read captures its full output." },
                "max_bytes": { "type": "integer", "description": "Cap on bytes returned (optional)." }
            },
            "required": ["session_id"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let id = session_id_arg(&args)?;
        let max = usize::try_from(args.get("max_bytes").and_then(Json::as_u64).unwrap_or(0))
            .unwrap_or(usize::MAX);
        // Clamp the wait so an automation node can't block the worker indefinitely.
        let wait_secs = args
            .get("wait_secs")
            .and_then(Json::as_u64)
            .unwrap_or(0)
            .min(600);
        let bytes = self.manager.read_wait(ws, id, max, wait_secs).await?;
        Ok(json!({ "output": String::from_utf8_lossy(&bytes) }))
    }
}

/// `list_terminals` — list a workspace's active sessions.
struct ListTerminalsTool {
    manager: Arc<TerminalManager>,
}

#[async_trait]
impl Tool for ListTerminalsTool {
    fn name(&self) -> &str {
        "list_terminals"
    }
    fn required_capability(&self) -> Option<Capability> {
        exec_cap()
    }
    fn description(&self) -> &str {
        "List the active terminal sessions in this workspace."
    }
    fn parameters_schema(&self) -> Json {
        json!({ "type": "object", "properties": {} })
    }
    async fn invoke(&self, _args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let sessions = self.manager.list(ws).await?;
        Ok(json!(sessions
            .iter()
            .map(|s| json!({
                "session_id": s.id,
                "backend": s.backend.as_token(),
                "host_dir": s.host_dir,
            }))
            .collect::<Vec<_>>()))
    }
}

/// `close_terminal` — end a session.
struct CloseTerminalTool {
    manager: Arc<TerminalManager>,
}

#[async_trait]
impl Tool for CloseTerminalTool {
    fn name(&self) -> &str {
        "close_terminal"
    }
    fn required_capability(&self) -> Option<Capability> {
        exec_cap()
    }
    fn description(&self) -> &str {
        "Close a terminal session and free its resources. An ephemeral \
         session's temp dir is discarded (persist_terminal first to keep files)."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": { "session_id": { "type": "string", "description": "From open_terminal." } },
            "required": ["session_id"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let id = session_id_arg(&args)?;
        self.manager.close(ws, id).await?;
        Ok(json!({ "ok": true }))
    }
}

/// `persist_terminal` — flush a session's working dir to object storage.
struct PersistTerminalTool {
    manager: Arc<TerminalManager>,
}

#[async_trait]
impl Tool for PersistTerminalTool {
    fn name(&self) -> &str {
        "persist_terminal"
    }
    fn required_capability(&self) -> Option<Capability> {
        // `exec:run`, like the rest of the terminal surface (a protected scope no
        // base role holds, §19). Dispatch enforces a single capability, so we gate
        // on the security-critical scope rather than the weaker `storage:write` a
        // base Member already has. Only registered when an object store exists.
        exec_cap()
    }
    fn description(&self) -> &str {
        "Snapshot a terminal session's working directory to object storage under \
         `prefix` (e.g. \"runs/2026-06-25\"), optionally only the `source_subdir` \
         folder of it (e.g. a cloned repo's \"docs\"). Each written file is catalogued \
         and fires a storage-change event, so this can feed a downstream indexing \
         automation. Use for an ephemeral terminal whose files you want to keep. \
         Returns the object keys written."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "description": "From open_terminal." },
                "prefix": { "type": "string", "description": "Object-key prefix to write files under." },
                "source_subdir": { "type": "string", "description": "Optional: copy only this subdir of the working directory (relative, no '..'). Omit to copy the whole workdir." }
            },
            "required": ["session_id", "prefix"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let id = session_id_arg(&args)?;
        let prefix = args
            .get("prefix")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::invalid("`prefix` is required"))?;
        let source_subdir = args.get("source_subdir").and_then(Json::as_str);
        let keys = self.manager.persist(ws, id, prefix, source_subdir).await?;
        Ok(json!({ "keys": keys }))
    }
}

/// `stage_object` — copy a stored file from a files store **into** a terminal
/// session's working directory (the inverse of `persist_terminal`, SOUL §9/§20).
/// Holds the [`StorageRegistry`] + [`Store`] so it can resolve *any* source store
/// the same way the `/storage` routes do — including the caller's per-user default
/// — so a file on a different backend than the terminal's folder (e.g. an S3 files
/// store vs. a local workdir) can be pulled in. Gated on `exec:run` like the rest
/// of the terminal surface; only registered when an object store exists.
struct StageObjectTool {
    manager: Arc<TerminalManager>,
    storage: StorageRegistry,
    store: Store,
}

impl StageObjectTool {
    /// Stage the optional `files` and recursive `directories` supplied directly
    /// to `open_terminal`. Per-item failures are returned alongside successes so
    /// a typo never hides/leaks the already-open live session. Directory expansion
    /// is bounded across the whole request to avoid accidentally copying a store.
    async fn stage_open_requests(
        &self,
        args: &Json,
        ws: WorkspaceId,
        id: TerminalSessionId,
        ctx: &ToolContext,
    ) -> Json {
        let mut results = Vec::new();
        let mut staged_count = 0usize;

        if let Some(files) = args.get("files") {
            match files.as_array() {
                Some(files) => {
                    for spec in files {
                        if staged_count >= MAX_OPEN_STAGE_FILES {
                            results.push(json!({
                                "ok": false,
                                "kind": "limit",
                                "error": format!("open_terminal stages at most {MAX_OPEN_STAGE_FILES} files")
                            }));
                            break;
                        }
                        let mut result = match self.stage_one(spec, ws, id, ctx).await {
                            Ok(value) => value,
                            Err(error) => json!({ "ok": false, "error": error.to_string() }),
                        };
                        if result.get("ok").and_then(Json::as_bool) == Some(true) {
                            staged_count += 1;
                        }
                        if let Some(object) = result.as_object_mut() {
                            object.insert("kind".into(), json!("file"));
                        }
                        results.push(result);
                    }
                }
                None => results.push(json!({
                    "ok": false,
                    "kind": "files",
                    "error": "`files` must be an array"
                })),
            }
        }

        if let Some(directories) = args.get("directories") {
            match directories.as_array() {
                Some(directories) => {
                    for spec in directories {
                        if staged_count >= MAX_OPEN_STAGE_FILES {
                            results.push(json!({
                                "ok": false,
                                "kind": "limit",
                                "error": format!("open_terminal stages at most {MAX_OPEN_STAGE_FILES} files")
                            }));
                            break;
                        }
                        match self
                            .stage_open_directory(
                                spec,
                                ws,
                                id,
                                ctx,
                                MAX_OPEN_STAGE_FILES - staged_count,
                            )
                            .await
                        {
                            Ok(mut directory_results) => {
                                staged_count += directory_results
                                    .iter()
                                    .filter(|item| {
                                        item.get("ok").and_then(Json::as_bool) == Some(true)
                                    })
                                    .count();
                                results.append(&mut directory_results);
                            }
                            Err(error) => results.push(json!({
                                "ok": false,
                                "kind": "directory",
                                "prefix": spec.get("prefix").cloned().unwrap_or(Json::Null),
                                "error": error.to_string()
                            })),
                        }
                    }
                }
                None => results.push(json!({
                    "ok": false,
                    "kind": "directories",
                    "error": "`directories` must be an array"
                })),
            }
        }

        json!({
            "requested": args.get("files").is_some() || args.get("directories").is_some(),
            "staged_files": staged_count,
            "results": results,
        })
    }

    /// Expand one store prefix and stage every object while preserving its path
    /// below `dest_dir` (or below the source prefix's final directory name).
    async fn stage_open_directory(
        &self,
        spec: &Json,
        ws: WorkspaceId,
        id: TerminalSessionId,
        ctx: &ToolContext,
        remaining: usize,
    ) -> Result<Vec<Json>> {
        let prefix = spec
            .get("prefix")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|value| !value.trim_matches('/').is_empty())
            .ok_or_else(|| Error::invalid("`directories[].prefix` must be non-empty"))?;
        let source_prefix = format!("{}/", prefix.trim_matches('/'));
        let store_name = spec
            .get("store")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let default_dest = source_prefix
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or_default();
        let dest_dir = spec
            .get("dest_dir")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(default_dest)
            .trim_matches('/');
        if dest_dir.is_empty() {
            return Err(Error::invalid("`directories[].dest_dir` must be non-empty"));
        }

        let handle = crate::routes::storage::resolve_store(
            &self.storage,
            &self.store,
            ws,
            ctx.user_id,
            store_name,
        )
        .await
        .map_err(|error| Error::other(error.to_string()))?;
        let physical_prefix = handle.physical_key(ws, &source_prefix);
        let mut stream = handle.backend.list(&physical_prefix).await?;
        let mut keys = Vec::new();
        while let Some(item) = stream.next().await {
            let meta = item?;
            keys.push(handle.user_key(ws, &meta.key));
            if keys.len() > remaining {
                return Err(Error::invalid(format!(
                    "directory `{source_prefix}` exceeds the remaining open_terminal staging limit of {remaining} files"
                )));
            }
        }
        if keys.is_empty() {
            return Err(Error::invalid(format!(
                "no stored files under directory `{source_prefix}`"
            )));
        }
        keys.sort();

        let mut results = Vec::with_capacity(keys.len());
        for key in keys {
            let Some(relative) = key.strip_prefix(&source_prefix) else {
                continue;
            };
            if relative.is_empty() {
                continue;
            }
            let dest_path = format!("{dest_dir}/{relative}");
            let request = json!({
                "key": key,
                "store": handle.store,
                "dest_path": dest_path,
            });
            let mut result = match self.stage_one(&request, ws, id, ctx).await {
                Ok(value) => value,
                Err(error) => json!({
                    "ok": false,
                    "key": request.get("key"),
                    "path": request.get("dest_path"),
                    "error": error.to_string()
                }),
            };
            if let Some(object) = result.as_object_mut() {
                object.insert("kind".into(), json!("directory_file"));
                object.insert("source_prefix".into(), json!(source_prefix));
            }
            results.push(result);
        }
        Ok(results)
    }

    /// Resolve one source store and stream a single object (`{key, store?,
    /// dest_path?}`) into the already-opened session at `id`. Shared by the
    /// single-object form and each entry of the batch (`items`) form so both take
    /// the same fields and defaulting (unnamed store → per-user default, `dest_path`
    /// → the key's filename).
    async fn stage_one(
        &self,
        spec: &Json,
        ws: WorkspaceId,
        id: TerminalSessionId,
        ctx: &ToolContext,
    ) -> Result<Json> {
        let key = spec
            .get("key")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::invalid("`key` is required"))?;
        let store = spec
            .get("store")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let dest_path = spec
            .get("dest_path")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        // A session owned by a peer pod stages there (SOUL §16 M7): the workdir
        // lives on the owner's filesystem, and the object store is shared, so the
        // owner resolves the store + streams the bytes itself.
        if let Some(pod) = self.manager.remote_owner(ws, id).await {
            return self
                .manager
                .forward_remote(
                    &pod,
                    ws,
                    id,
                    PodOp::StageObject {
                        store: store.map(str::to_string),
                        key: key.to_string(),
                        dest_path: dest_path.map(str::to_string),
                        user_id: ctx.user_id,
                    },
                )
                .await;
        }
        stage_object_via(
            &self.manager,
            &self.storage,
            &self.store,
            ws,
            id,
            ctx.user_id,
            store,
            key,
            dest_path,
        )
        .await
    }
}

/// Resolve one source store and stream a single object into the session at
/// `id` — the body shared by the `stage_object` tool and the owner side of a
/// forwarded [`PodOp::StageObject`], so both paths default identically (unnamed
/// store → the caller's per-user default; `dest_path` → the key's filename).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn stage_object_via(
    manager: &TerminalManager,
    storage: &StorageRegistry,
    store_db: &Store,
    ws: WorkspaceId,
    id: TerminalSessionId,
    user_id: Option<catalerum_core::UserId>,
    store: Option<&str>,
    key: &str,
    dest_path: Option<&str>,
) -> Result<Json> {
    let dest_path = dest_path
        .map(str::to_string)
        .unwrap_or_else(|| key.rsplit('/').next().unwrap_or(key).to_string());
    // Resolve the source store the same way an HTTP handler does (honouring the
    // caller's per-user default when `store` is omitted), then stream its bytes
    // into the session workdir.
    let handle = crate::routes::storage::resolve_store(storage, store_db, ws, user_id, store)
        .await
        .map_err(|e| Error::other(e.to_string()))?;
    let physical = handle.physical_key(ws, key);
    let bytes = manager
        .stage_object(ws, id, handle.backend.as_ref(), &physical, &dest_path)
        .await?;
    Ok(json!({
        "ok": true,
        "path": dest_path,
        "bytes": bytes,
        "store": handle.store,
        "key": key,
        "session_id": id,
        "next_tools": ["terminal_write", "terminal_read"],
        "usage_note": "Run commands against this path with terminal_write in this same session, then collect output with terminal_read. run_command cannot see this terminal workdir.",
    }))
}

#[async_trait]
impl Tool for StageObjectTool {
    fn name(&self) -> &str {
        "stage_object"
    }
    fn required_capability(&self) -> Option<Capability> {
        exec_cap()
    }
    fn description(&self) -> &str {
        "Copy a stored file from a files store into a terminal session's working \
         directory so the shell can operate on it — the inverse of persist_terminal. \
         Use when a file lives in object storage (e.g. a chat upload) but the \
         terminal works on a local folder. `key` is the store-relative object key \
         (from an attachment reference or query_structured); omit `store` to use \
         your default files store. `dest_path` defaults to the key's filename. \
         Returns the bytes written and the workdir path. Stage several files into the \
         same session at once by passing an `items` array instead of the top-level \
         key/store/dest_path fields. After staging, operate on the path with \
         terminal_write and terminal_read using this same `session_id`; run_command \
         has a different working-directory context and cannot see session files."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "description": "From open_terminal." },
                "key": { "type": "string", "description": "Object key within the store (store-relative). Single-stage form; omit when using `items`." },
                "store": { "type": "string", "description": "Source store name; omitted → your default files store." },
                "dest_path": { "type": "string", "description": "Destination path relative to the session workdir; omitted → the key's filename." },
                "items": {
                    "type": "array",
                    "description": "Batch form: stage several objects into this same session in one call. Each entry takes the same key/store/dest_path fields as a single stage. When present, the top-level key/store/dest_path fields are ignored and the result is a `results` array (one entry per item, in order, each carrying an `ok` flag — failures don't abort the rest).",
                    "items": {
                        "type": "object",
                        "properties": {
                            "key": { "type": "string", "description": "Object key within the store (store-relative)." },
                            "store": { "type": "string", "description": "Source store name; omitted → your default files store." },
                            "dest_path": { "type": "string", "description": "Destination path relative to the session workdir; omitted → the key's filename." }
                        },
                        "required": ["key"]
                    }
                }
            },
            "required": ["session_id"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let id = session_id_arg(&args)?;
        // Batch form: `items` stages each spec into this session in order, reporting
        // per-item success so a bad entry surfaces without aborting (or hiding) the
        // others — staging is not transactional across items.
        if let Some(items) = args.get("items").and_then(Json::as_array) {
            if items.is_empty() {
                return Err(Error::invalid("`items` must not be empty"));
            }
            let mut results = Vec::with_capacity(items.len());
            for (index, spec) in items.iter().enumerate() {
                let entry = match self.stage_one(spec, ws, id, ctx).await {
                    Ok(mut v) => {
                        if let Some(obj) = v.as_object_mut() {
                            obj.insert("index".into(), json!(index));
                        }
                        v
                    }
                    Err(e) => json!({ "ok": false, "index": index, "error": e.to_string() }),
                };
                results.push(entry);
            }
            return Ok(json!({ "results": results }));
        }
        // Single form (unchanged shape for existing callers).
        self.stage_one(&args, ws, id, ctx).await
    }
}

/// `store_object` — copy a file **out of** a terminal session's working directory
/// into a files store (the inverse of `stage_object`, SOUL §9/§20). Like
/// [`StageObjectTool`] it holds the [`StorageRegistry`] + [`Store`] so it can resolve
/// *any* destination store the same way the `/storage` routes do (including the
/// caller's per-user default), then catalogues the written object + fires the
/// `StorageObject` trigger exactly like an upload so the file is queryable and can
/// head a downstream automation. Gated on `exec:run`; only registered with a store.
struct StoreObjectTool {
    manager: Arc<TerminalManager>,
    storage: StorageRegistry,
    store: Store,
}

impl StoreObjectTool {
    /// Resolve one destination store and stream a single workdir file (`{path,
    /// store?, key?}`) out of the session at `id` into it. Shared by the single-file
    /// and batch (`items`) forms so both take the same fields and defaulting (unnamed
    /// store → per-user default, `key` → the source path's filename).
    async fn store_one(
        &self,
        spec: &Json,
        ws: WorkspaceId,
        id: TerminalSessionId,
        ctx: &ToolContext,
    ) -> Result<Json> {
        let path = spec
            .get("path")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::invalid("`path` is required"))?;
        let store = spec
            .get("store")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let key = spec
            .get("key")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        // A session owned by a peer pod stores from there (SOUL §16 M7): the
        // workdir file lives on the owner's filesystem, and the object store is
        // shared, so the owner resolves the store + streams the bytes itself.
        if let Some(pod) = self.manager.remote_owner(ws, id).await {
            return self
                .manager
                .forward_remote(
                    &pod,
                    ws,
                    id,
                    PodOp::StoreObject {
                        store: store.map(str::to_string),
                        key: key.map(str::to_string),
                        path: path.to_string(),
                        user_id: ctx.user_id,
                    },
                )
                .await;
        }
        store_object_via(
            &self.manager,
            &self.storage,
            &self.store,
            ws,
            id,
            ctx.user_id,
            store,
            key,
            path,
        )
        .await
    }
}

/// Resolve one destination store and stream a single workdir file out of the
/// session at `id` into it — the body shared by the `store_object` tool and the
/// owner side of a forwarded [`PodOp::StoreObject`], so both paths default
/// identically (unnamed store → the caller's per-user default; `key` → the
/// source path's filename).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn store_object_via(
    manager: &TerminalManager,
    storage: &StorageRegistry,
    store_db: &Store,
    ws: WorkspaceId,
    id: TerminalSessionId,
    user_id: Option<catalerum_core::UserId>,
    store: Option<&str>,
    key: Option<&str>,
    path: &str,
) -> Result<Json> {
    let key = key
        .map(str::to_string)
        .unwrap_or_else(|| path.rsplit('/').next().unwrap_or(path).to_string());
    // Resolve the destination store the same way an HTTP handler does (honouring
    // the caller's per-user default when `store` is omitted), stream the workdir
    // file into it, then catalogue + notify exactly like an upload.
    let handle = crate::routes::storage::resolve_store(storage, store_db, ws, user_id, store)
        .await
        .map_err(|e| Error::other(e.to_string()))?;
    let physical = handle.physical_key(ws, &key);
    let bytes = manager
        .store_object(ws, id, handle.backend.as_ref(), &physical, path)
        .await?;
    // Re-stat for the authoritative size/etag/content-type and catalogue the
    // user-facing key (never the physical namespaced one). Best-effort: the bytes
    // are already stored, so a stat/catalogue failure is logged, not surfaced.
    match handle.backend.stat(&physical).await {
        Ok(mut object) => {
            object.key = key.clone();
            crate::routes::storage::catalogue_and_notify(
                store_db,
                ws,
                &handle.connection,
                &handle.bucket,
                &object,
            )
            .await;
        }
        Err(e) => {
            tracing::warn!(error = %e, key = %key,
                "failed to stat stored object for catalogue (bytes stored)");
        }
    }
    Ok(json!({
        "ok": true,
        "path": path,
        "bytes": bytes,
        "store": handle.store,
        "key": key,
    }))
}

#[async_trait]
impl Tool for StoreObjectTool {
    fn name(&self) -> &str {
        "store_object"
    }
    fn required_capability(&self) -> Option<Capability> {
        exec_cap()
    }
    fn description(&self) -> &str {
        "Copy a file out of a terminal session's working directory into a files store \
         — the inverse of stage_object. Use to hand a file the shell produced (a \
         build artifact, a generated report) back to object storage so it's \
         catalogued, searchable, and can head a downstream automation. `path` is the \
         file's path relative to the session workdir; omit `store` to use your \
         default files store; `key` defaults to the source path's filename. Returns \
         the bytes written and the destination key. Store several files out of the \
         same session at once by passing an `items` array instead of the top-level \
         path/store/key fields."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "description": "From open_terminal." },
                "path": { "type": "string", "description": "Source file path relative to the session workdir. Single-store form; omit when using `items`." },
                "store": { "type": "string", "description": "Destination store name; omitted → your default files store." },
                "key": { "type": "string", "description": "Destination object key within the store (store-relative); omitted → the source path's filename." },
                "items": {
                    "type": "array",
                    "description": "Batch form: store several workdir files out of this same session in one call. Each entry takes the same path/store/key fields as a single store. When present, the top-level path/store/key fields are ignored and the result is a `results` array (one entry per item, in order, each carrying an `ok` flag — failures don't abort the rest).",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string", "description": "Source file path relative to the session workdir." },
                            "store": { "type": "string", "description": "Destination store name; omitted → your default files store." },
                            "key": { "type": "string", "description": "Destination object key within the store (store-relative); omitted → the source path's filename." }
                        },
                        "required": ["path"]
                    }
                }
            },
            "required": ["session_id"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let id = session_id_arg(&args)?;
        // Batch form: `items` stores each spec out of this session in order, reporting
        // per-item success so a bad entry surfaces without aborting (or hiding) the
        // others — storing is not transactional across items.
        if let Some(items) = args.get("items").and_then(Json::as_array) {
            if items.is_empty() {
                return Err(Error::invalid("`items` must not be empty"));
            }
            let mut results = Vec::with_capacity(items.len());
            for (index, spec) in items.iter().enumerate() {
                let entry = match self.store_one(spec, ws, id, ctx).await {
                    Ok(mut v) => {
                        if let Some(obj) = v.as_object_mut() {
                            obj.insert("index".into(), json!(index));
                        }
                        v
                    }
                    Err(e) => json!({ "ok": false, "index": index, "error": e.to_string() }),
                };
                results.push(entry);
            }
            return Ok(json!({ "results": results }));
        }
        // Single form.
        self.store_one(&args, ws, id, ctx).await
    }
}

/// `read_file` — read a text file from a terminal session's working directory.
struct ReadFileTool {
    manager: Arc<TerminalManager>,
}

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }
    fn required_capability(&self) -> Option<Capability> {
        exec_cap()
    }
    fn description(&self) -> &str {
        "Read a text file from a terminal session's working directory. `path` is \
         relative to that directory (the `host_dir` from open_terminal). Window a \
         large file with `offset` (1-based start line) and `limit` (max lines). \
         Returns the file `content` plus `total_lines`, on-disk `size`, and a \
         `truncated` flag. Prefer this over `cat` in terminal_write — it returns \
         the text directly instead of racing the PTY output."
    }
    fn description_for(&self, input_modalities: &[String]) -> String {
        read_file_description(self.description(), input_modalities)
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "description": "From open_terminal." },
                "path": { "type": "string", "description": "File path relative to the session's working directory." },
                "offset": { "type": "integer", "description": "1-based line to start from (optional)." },
                "limit": { "type": "integer", "description": "Max lines to return (optional)." }
            },
            "required": ["session_id", "path"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        self.invoke_for_model(args, ctx, &[]).await
    }
    async fn invoke_for_model(
        &self,
        args: Json,
        ctx: &ToolContext,
        input_modalities: &[String],
    ) -> Result<Json> {
        let ws = workspace(ctx)?;
        let id = session_id_arg(&args)?;
        let path = path_arg(&args)?;
        let offset = args
            .get("offset")
            .and_then(Json::as_u64)
            .map(|n| n as usize)
            .filter(|n| *n > 0);
        let limit = args.get("limit").and_then(Json::as_u64).map(|n| n as usize);
        if let Some(content_type) = supported_media_path(path, input_modalities) {
            if offset.is_some() || limit.is_some() {
                return Err(Error::invalid(
                    "`offset`/`limit` cannot be used when ingesting binary media",
                ));
            }
            let media = self.manager.read_media_file(ws, id, path).await?;
            let encoded = b64(&media.bytes);
            let input = MediaInput::Image {
                url: format!("data:{content_type};base64,{encoded}"),
            };
            return Ok(json!({
                "path": path,
                "content_type": content_type,
                "size": media.size,
                "ingested": true,
                MODEL_MEDIA_RESULT_FIELD: [input],
            }));
        }
        let r = self.manager.read_file(ws, id, path, offset, limit).await?;
        Ok(json!({
            "content": r.content,
            "total_lines": r.total_lines,
            "size": r.size,
            "truncated": r.truncated,
        }))
    }
}

fn supported_media_path(path: &str, input_modalities: &[String]) -> Option<&'static str> {
    let content_type = mime_guess::from_path(path).first_raw()?;
    (content_type.starts_with("image/")
        && input_modalities
            .iter()
            .any(|modality| modality.eq_ignore_ascii_case("image")))
    .then_some(content_type)
}

fn read_file_description(base: &str, input_modalities: &[String]) -> String {
    let supports_images = input_modalities
        .iter()
        .any(|modality| modality.eq_ignore_ascii_case("image"));
    if !supports_images {
        return format!(
            "{base} Binary files are rejected; native binary ingestion is unavailable for the \
             active model through llmleaf."
        );
    }
    format!(
        "{base} Binary files are rejected by default. Because the active model accepts image input, \
         a recognized image file is instead ingested natively through llmleaf and attached to the \
         next model turn; do not use `offset`/`limit` for images.",
    )
}

/// `create_file` — write a text file into a terminal session's working directory.
struct CreateFileTool {
    manager: Arc<TerminalManager>,
}

#[async_trait]
impl Tool for CreateFileTool {
    fn name(&self) -> &str {
        "create_file"
    }
    fn required_capability(&self) -> Option<Capability> {
        exec_cap()
    }
    fn description(&self) -> &str {
        "Create (or overwrite) a text file in a terminal session's working \
         directory, creating any parent directories. `path` is relative to that \
         directory. Use this to author code/config the terminal will run, instead \
         of heredocs in terminal_write. Returns the bytes written and whether an \
         existing file was overwritten. Use edit_file for targeted changes."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "description": "From open_terminal." },
                "path": { "type": "string", "description": "File path relative to the session's working directory (parent dirs are created)." },
                "content": { "type": "string", "description": "The file's full contents." }
            },
            "required": ["session_id", "path", "content"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let id = session_id_arg(&args)?;
        let path = path_arg(&args)?;
        let content = args
            .get("content")
            .and_then(Json::as_str)
            .ok_or_else(|| Error::invalid("`content` is required"))?;
        let (bytes_written, overwrote) = self.manager.write_file(ws, id, path, content).await?;
        Ok(json!({
            "ok": true,
            "path": path,
            "bytes_written": bytes_written,
            "created": !overwrote,
        }))
    }
}

/// `edit_file` — replace text in an existing file in a session's working directory.
struct EditFileTool {
    manager: Arc<TerminalManager>,
}

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }
    fn required_capability(&self) -> Option<Capability> {
        exec_cap()
    }
    fn description(&self) -> &str {
        "Replace text in an existing file in a terminal session's working \
         directory. `old_string` must appear exactly once unless `replace_all` is \
         true (then every occurrence is replaced). `path` is relative to the \
         working directory. Returns the number of replacements. Use create_file to \
         write a whole file."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "description": "From open_terminal." },
                "path": { "type": "string", "description": "File path relative to the session's working directory." },
                "old_string": { "type": "string", "description": "Exact text to replace (must be unique unless replace_all)." },
                "new_string": { "type": "string", "description": "Replacement text." },
                "replace_all": { "type": "boolean", "description": "Replace every occurrence (default false)." }
            },
            "required": ["session_id", "path", "old_string", "new_string"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let id = session_id_arg(&args)?;
        let path = path_arg(&args)?;
        let old = args
            .get("old_string")
            .and_then(Json::as_str)
            .ok_or_else(|| Error::invalid("`old_string` is required"))?;
        let new = args
            .get("new_string")
            .and_then(Json::as_str)
            .ok_or_else(|| Error::invalid("`new_string` is required"))?;
        let replace_all = args
            .get("replace_all")
            .and_then(Json::as_bool)
            .unwrap_or(false);
        let replacements = self
            .manager
            .edit_file(ws, id, path, old, new, replace_all)
            .await?;
        Ok(json!({ "ok": true, "path": path, "replacements": replacements }))
    }
}

// ---------------------------------------------------------------------------
// terminal_subagent — isolated coding worker + profile-style Boa policy
// ---------------------------------------------------------------------------

const TERMINAL_SUBAGENT_NAME: &str = "terminal_subagent";
const TERMINAL_UPSTREAM_NAME: &str = "upstream";
const TERMINAL_FINISH_NAME: &str = "finish_work";
const MAX_SUBAGENT_SCRIPT_BYTES: usize = 64 * 1024;
const TERMINAL_SUBAGENT_TOOLS: &[&str] = &[
    "terminal_write",
    "terminal_read",
    "read_file",
    "create_file",
    "edit_file",
];

/// Pin an existing terminal tool to the session prepared by the parent. The
/// child never receives `open_terminal`, staging, persistence, or session-list
/// tools and cannot redirect an allowed operation to another session id.
struct PinnedTerminalTool {
    inner: Arc<dyn Tool>,
    session_id: TerminalSessionId,
    description: String,
}

#[async_trait]
impl Tool for PinnedTerminalTool {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn required_capability(&self) -> Option<Capability> {
        self.inner.required_capability()
    }

    fn parameters_schema(&self) -> Json {
        let mut schema = self.inner.parameters_schema();
        if let Some(properties) = schema.get_mut("properties").and_then(Json::as_object_mut) {
            properties.remove("session_id");
        }
        if let Some(required) = schema.get_mut("required").and_then(Json::as_array_mut) {
            required.retain(|value| value.as_str() != Some("session_id"));
        }
        schema
    }

    async fn invoke(&self, mut args: Json, ctx: &ToolContext) -> Result<Json> {
        let object = args
            .as_object_mut()
            .ok_or_else(|| Error::invalid("terminal tool arguments must be an object"))?;
        object.insert("session_id".into(), json!(self.session_id));
        self.inner.invoke(args, ctx).await
    }
}

/// The child's sole route back to the parent's registry. The proxy has a static
/// parent-selected tool-name allow-list; the profile-compatible Boa gate layered
/// on the child then inspects `input.args.tool` and
/// `input.args.arguments` to constrain exact PR/story identifiers and operations.
struct TerminalUpstreamTool {
    description: String,
    allowed_tools: Vec<String>,
    parent_registry: ToolRegistry,
    parent_ctx: ToolContext,
    cancel: CancellationToken,
}

impl TerminalUpstreamTool {
    fn recursive(tool: &str) -> bool {
        matches!(
            tool,
            "run_javascript"
                | "delegate"
                | "open_terminal"
                | "terminal_subagent"
                | "computer_subagent"
                | "computer_agent_task"
                | "monitor_subagent"
                | "wait_subagent"
                | "stop_subagent"
        )
    }

    fn blocked(tool: &str) -> bool {
        Self::recursive(tool)
            || matches!(
                tool,
                "run_command"
                    | "terminal_write"
                    | "terminal_read"
                    | "read_file"
                    | "create_file"
                    | "edit_file"
                    | "list_terminals"
                    | "close_terminal"
                    | "persist_terminal"
                    | "stage_object"
                    | "store_object"
            )
            || tool.starts_with("computer_")
    }
}

#[async_trait]
impl Tool for TerminalUpstreamTool {
    fn name(&self) -> &str {
        TERMINAL_UPSTREAM_NAME
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "tool": {
                    "type": "string",
                    "enum": self.allowed_tools,
                    "description": "The parent tool to request through the policy boundary."
                },
                "arguments": {
                    "type": "object",
                    "description": "Arguments for that parent tool. The Boa policy inspects these before dispatch."
                }
            },
            "required": ["tool", "arguments"]
        })
    }

    async fn invoke(&self, args: Json, _ctx: &ToolContext) -> Result<Json> {
        if self.cancel.is_cancelled() {
            return Err(Error::other("terminal subagent was stopped"));
        }
        let tool = args
            .get("tool")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Error::invalid("upstream requires `tool`"))?;
        if Self::blocked(tool) || !self.allowed_tools.iter().any(|allowed| allowed == tool) {
            return Err(Error::unauthorized(format!(
                "`{tool}` is not on this terminal subagent's upstream allow-list"
            )));
        }
        let arguments = args
            .get("arguments")
            .cloned()
            .filter(Json::is_object)
            .ok_or_else(|| Error::invalid("upstream `arguments` must be an object"))?;
        self.parent_registry
            .dispatch(tool, arguments, &self.parent_ctx)
            .await
    }
}

#[derive(Clone, Debug)]
struct GoalVerdict {
    met: bool,
    analysis: String,
    evidence: Vec<String>,
    summary: String,
}

impl GoalVerdict {
    fn as_json(&self) -> Json {
        json!({
            "met": self.met,
            "analysis": self.analysis,
            "evidence": self.evidence,
            "summary": self.summary,
        })
    }
}

fn finalizer_reported_push(goal_met: bool, result: &Json, error: Option<&str>) -> bool {
    goal_met
        && error.is_none()
        && result
            .get("pushed")
            .and_then(Json::as_bool)
            .unwrap_or(false)
}

/// A structured terminal condition for the child run. The finalizer never
/// trusts free-form assistant prose: it runs only when this tool recorded
/// `goal_met=true`, with the analysis + evidence handed to the parent script.
struct FinishTerminalWorkTool {
    verdict: Arc<Mutex<Option<GoalVerdict>>>,
}

#[async_trait]
impl Tool for FinishTerminalWorkTool {
    fn name(&self) -> &str {
        TERMINAL_FINISH_NAME
    }

    fn description(&self) -> &str {
        "Finish the assigned work with a structured goal assessment. Call exactly once after \
         inspecting the diff and running the available verification. Set goal_met=true only \
         when the evidence demonstrates the requested goal is satisfied. Only a true verdict \
         permits the parent-side finalizer to publish upstream."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "goal_met": { "type": "boolean" },
                "analysis": { "type": "string", "description": "Why the goal is or is not met, tied to acceptance criteria." },
                "evidence": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Concrete evidence such as tests, diff inspection, and remaining failures."
                },
                "summary": { "type": "string", "description": "Concise implementation summary for the parent/PR." }
            },
            "required": ["goal_met", "analysis", "evidence", "summary"]
        })
    }

    async fn invoke(&self, args: Json, _ctx: &ToolContext) -> Result<Json> {
        let met = args
            .get("goal_met")
            .and_then(Json::as_bool)
            .ok_or_else(|| Error::invalid("finish_work requires boolean `goal_met`"))?;
        let required_text = |name: &str| -> Result<String> {
            args.get(name)
                .and_then(Json::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .ok_or_else(|| Error::invalid(format!("finish_work requires non-empty `{name}`")))
        };
        let evidence = args
            .get("evidence")
            .and_then(Json::as_array)
            .ok_or_else(|| Error::invalid("finish_work requires `evidence` array"))?
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
                    .ok_or_else(|| {
                        Error::invalid("finish_work evidence entries must be non-empty strings")
                    })
            })
            .collect::<Result<Vec<_>>>()?;
        if evidence.is_empty() {
            return Err(Error::invalid(
                "finish_work requires at least one evidence entry",
            ));
        }
        let verdict = GoalVerdict {
            met,
            analysis: required_text("analysis")?,
            evidence,
            summary: required_text("summary")?,
        };
        let mut slot = self
            .verdict
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if slot.is_some() {
            return Err(Error::invalid("finish_work may be called only once"));
        }
        *slot = Some(verdict.clone());
        Ok(json!({ "recorded": true, "goal": verdict.as_json() }))
    }
}

/// Boa host for the post-goal parent finalizer. Calls run through the parent's
/// exact registry/context, so static capabilities, the parent's own tool guard,
/// dry-run simulation, and approvals still apply. Recursive agent/Boa entry
/// points remain blocked.
struct TerminalFinalizerHost {
    registry: ToolRegistry,
    parent_ctx: ToolContext,
    cancel: CancellationToken,
}

impl catalerum_script::UiScriptHost for TerminalFinalizerHost {
    fn call_tool(&self, tool: &str, args: Json) -> std::result::Result<Json, String> {
        if self.cancel.is_cancelled() {
            return Err("terminal subagent was stopped; finalization is disabled".into());
        }
        if TerminalUpstreamTool::recursive(tool) {
            return Err(format!(
                "terminal finalizer cannot call `{tool}` — recursive agent/Boa execution is disabled"
            ));
        }
        tokio::runtime::Handle::current()
            .block_on(self.registry.dispatch(tool, args, &self.parent_ctx))
            .map_err(|error| error.to_string())
    }
}

fn parse_string_array(args: &Json, name: &str) -> Result<Vec<String>> {
    let values = args
        .get(name)
        .and_then(Json::as_array)
        .ok_or_else(|| Error::invalid(format!("terminal_subagent requires `{name}` array")))?;
    let mut out = Vec::with_capacity(values.len());
    for value in values {
        let value = value
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                Error::invalid(format!(
                    "terminal_subagent `{name}` entries must be non-empty strings"
                ))
            })?;
        if !out.iter().any(|existing| existing == value) {
            out.push(value.to_string());
        }
    }
    if out.is_empty() {
        return Err(Error::invalid(format!(
            "terminal_subagent requires at least one `{name}` entry"
        )));
    }
    Ok(out)
}

fn restricted_terminal_subagent_registry(
    parent: &ToolRegistry,
    session_id: TerminalSessionId,
    upstream: Arc<dyn Tool>,
    finish: Arc<dyn Tool>,
) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    for name in TERMINAL_SUBAGENT_TOOLS {
        if let Some(inner) = parent.get(name) {
            let description = format!(
                "{} This subagent-scoped instance is pinned to terminal `{session_id}`; no \
                 `session_id` argument is accepted.",
                inner.description()
            );
            registry.register(Arc::new(PinnedTerminalTool {
                inner,
                session_id,
                description,
            }));
        }
    }
    registry.register(upstream);
    registry.register(finish);
    registry
}

#[derive(Clone)]
struct TerminalSubagentTool {
    manager: Arc<TerminalManager>,
    store: Store,
    client: OpenRouterClient,
    default_model: String,
    subagent_runs: SubagentRunManager,
    /// Set only on the private tool clone owned by a background run.
    run_cancel: Option<CancellationToken>,
}

#[async_trait]
impl Tool for TerminalSubagentTool {
    fn name(&self) -> &str {
        TERMINAL_SUBAGENT_NAME
    }

    fn required_capability(&self) -> Option<Capability> {
        exec_cap()
    }

    fn description(&self) -> &str {
        "Run a coding subagent in an existing no-egress terminal after the parent has acquired a \
         central work item, created its PR, and staged the repository/repositories. The child sees \
         only terminal operations pinned to that session, `upstream`, and `finish_work`. `upstream` \
         can call only the parent tool names in `upstream_tools`; a profile-compatible Boa \
         `guard_script` reviews EVERY child call and output with `input.phase`, `input.tool`, \
         `input.args`, `input.output`, and immutable `input.policy_context`, so it can enforce the \
         exact PR and user-story ids and read/write operations. The child cannot publish directly \
         because its terminal must be OS network-isolated. It records a structured goal verdict; \
         only when goal_met=true does the parent-authored `finalize_code` run, under the parent's \
         original authority, to update/push upstream. The finalizer must return `{pushed: true}` \
         only after publication actually succeeds. Pass `profile` to use a named agent profile's \
         model, instructions, skills, tool restrictions, guard, and attenuated grant. Set \
         `background=true` to return a run id \
         immediately; the parent can monitor, wait for, or stop that run."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "description": "Existing no-egress terminal prepared with the repositories." },
                "goal": { "type": "string", "description": "Implementation goal and acceptance criteria from the user story." },
                "profile": {
                    "type": "string",
                    "description": "Optional named agent profile to configure and scope the subagent instead of using the workspace default model."
                },
                "upstream_tools": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Exact parent tool names the child may request through `upstream` (normally the PR and user-story provider tools only)."
                },
                "upstream_description": { "type": "string", "description": "Child-facing contract explaining the allowed PR/story operations and argument shapes." },
                "policy_context": {
                    "type": "object",
                    "description": "Immutable scope bound into every Boa review as `input.policy_context`, e.g. `{pull_request:{id,...}, user_story:{id,...}}`. The child cannot alter it."
                },
                "guard_script": {
                    "type": "string",
                    "description": "Profile-style Boa function body returning allow/deny/ask (or `{decision,reason}`). It reviews call and output phases and can inspect `input.args`, including `input.args.tool` + `input.args.arguments` for upstream requests. Failures deny."
                },
                "finalize_code": {
                    "type": "string",
                    "description": "Boa function body run by the parent only after `finish_work.goal_met=true`. It receives `input.goal`, `input.policy_context`, `input.session_id`, and `input.agent_content`; may call parent tools via `catalerum.callTool`; return an object with boolean `pushed` plus details."
                },
                "background": {
                    "type": "boolean",
                    "default": false,
                    "description": "Start on this API pod and return a controllable run id immediately."
                }
            },
            "required": ["session_id", "goal", "upstream_tools", "upstream_description", "policy_context", "guard_script", "finalize_code"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        if ctx.ui_id.is_some() {
            return Err(Error::unauthorized(
                "terminal_subagent is unavailable from an emerged-UI handler",
            ));
        }
        if self.run_cancel.is_none()
            && args
                .get("background")
                .and_then(Json::as_bool)
                .unwrap_or(false)
        {
            let goal = args
                .get("goal")
                .and_then(Json::as_str)
                .ok_or_else(|| Error::invalid("terminal_subagent requires `goal`"))?;
            let label = goal.chars().take(160).collect::<String>();
            let mut run_args = args;
            if let Some(object) = run_args.as_object_mut() {
                object.remove("background");
            }
            let template = self.clone();
            let parent_ctx = ctx.clone();
            return self
                .subagent_runs
                .spawn(
                    ctx,
                    TERMINAL_SUBAGENT_NAME,
                    label,
                    move |cancel| async move {
                        let runner: Arc<dyn Tool> = Arc::new(Self {
                            run_cancel: Some(cancel),
                            ..template
                        });
                        runner.invoke(run_args, &parent_ctx).await
                    },
                )
                .await;
        }
        let ws = workspace(ctx)?;
        let profile_name = args
            .get("profile")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let profile = if let Some(name) = profile_name {
            let parent_caps = ctx.capabilities.as_deref().ok_or_else(|| {
                Error::unauthorized("a named subagent profile requires a capability-scoped caller")
            })?;
            Some(
                resolve_constrained_profile(
                    &self.store,
                    ws,
                    &self.default_model,
                    name,
                    parent_caps,
                )
                .await?,
            )
        } else {
            None
        };
        let session_id = session_id_arg(&args)?;
        self.manager
            .require_subagent_isolation(ws, session_id)
            .await?;
        let goal = args
            .get("goal")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Error::invalid("terminal_subagent requires non-empty `goal`"))?
            .to_string();
        let upstream_tools = parse_string_array(&args, "upstream_tools")?;
        if upstream_tools
            .iter()
            .any(|tool| TerminalUpstreamTool::blocked(tool))
        {
            return Err(Error::invalid(
                "terminal_subagent `upstream_tools` contains a recursive/unsafe framework tool",
            ));
        }
        let parent_registry = ctx.registry.clone().ok_or_else(|| {
            Error::unauthorized(
                "terminal_subagent requires a dispatching parent registry (direct invoke refused)",
            )
        })?;
        for tool in &upstream_tools {
            if !parent_registry.contains(tool) {
                return Err(Error::invalid(format!(
                    "terminal_subagent upstream tool `{tool}` is not in the parent's registry"
                )));
            }
        }
        let upstream_description = args
            .get("upstream_description")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                Error::invalid("terminal_subagent requires non-empty `upstream_description`")
            })?
            .to_string();
        let policy_context = args
            .get("policy_context")
            .cloned()
            .filter(Json::is_object)
            .ok_or_else(|| {
                Error::invalid("terminal_subagent `policy_context` must be an object")
            })?;
        let guard_script = args
            .get("guard_script")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Error::invalid("terminal_subagent requires non-empty `guard_script`"))?
            .to_string();
        let finalize_code = args
            .get("finalize_code")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Error::invalid("terminal_subagent requires non-empty `finalize_code`"))?
            .to_string();
        for (name, source) in [
            ("guard_script", guard_script.as_str()),
            ("finalize_code", finalize_code.as_str()),
        ] {
            if source.len() > MAX_SUBAGENT_SCRIPT_BYTES {
                return Err(Error::invalid(format!(
                    "terminal_subagent `{name}` exceeds the {MAX_SUBAGENT_SCRIPT_BYTES}-byte limit"
                )));
            }
        }
        let cancel = self.run_cancel.clone().unwrap_or_default();

        let mut bound_policy_context = policy_context.clone();
        if let Some(object) = bound_policy_context.as_object_mut() {
            object.insert("session_id".into(), json!(session_id));
            object.insert("upstream_tools".into(), json!(upstream_tools));
        }
        let verdict = Arc::new(Mutex::new(None));
        let mut upstream_ctx = ctx.clone();
        if let Some(profile) = &profile {
            upstream_ctx.grant_id = profile.grant_id;
            upstream_ctx.capabilities = Some(profile.capabilities.clone());
            upstream_ctx.dry_run |= profile.dry_run;
        }
        let upstream: Arc<dyn Tool> = Arc::new(TerminalUpstreamTool {
            description: format!(
                "Parent-mediated PR/user-story channel. {upstream_description} Every request and \
                 response is inspected by the Boa policy before it is usable."
            ),
            allowed_tools: upstream_tools,
            parent_registry: parent_registry.clone(),
            parent_ctx: upstream_ctx,
            cancel: cancel.clone(),
        });
        let finish: Arc<dyn Tool> = Arc::new(FinishTerminalWorkTool {
            verdict: verdict.clone(),
        });
        let child_registry =
            restricted_terminal_subagent_registry(&parent_registry, session_id, upstream, finish);

        let guard = ToolGuard {
            script: Some(guard_script),
            llm: None,
            object_labels: None,
            on_error: GuardFail::Deny,
        };
        let mut child_ctx = ToolContext {
            workspace_id: Some(ws),
            user_id: ctx.user_id,
            agent_id: ctx.agent_id,
            grant_id: profile.as_ref().and_then(|profile| profile.grant_id),
            capabilities: profile
                .as_ref()
                .map(|profile| profile.capabilities.clone())
                .or_else(|| ctx.capabilities.clone()),
            dry_run: ctx.dry_run || profile.as_ref().is_some_and(|profile| profile.dry_run),
            gate: None,
            conversation_id: ctx.conversation_id,
            ui_id: None,
            registry: None,
        };
        let model = profile.as_ref().map_or_else(
            || self.default_model.clone(),
            |profile| profile.model.clone(),
        );
        let boundary_gate = crate::tool_gate::build_gate_with_context(
            Some(&guard),
            // Unlike a general profile, this constrained guard needs only its
            // bound policy context + optional classifyWithLlm. Giving its script
            // an empty lookup registry prevents the policy itself from bypassing
            // `upstream` or recursively spawning work through callTool.
            ToolRegistry::new(),
            self.store.clone(),
            ctx.clone(),
            self.client.clone(),
            model.clone(),
            Some(bound_policy_context.clone()),
        );
        let profile_gate = profile.as_ref().and_then(|profile| {
            crate::tool_gate::build_gate(
                profile.guard.as_ref(),
                child_registry.clone(),
                self.store.clone(),
                child_ctx.clone(),
                self.client.clone(),
                model.clone(),
            )
        });
        child_ctx.gate = crate::tool_gate::all_gates([boundary_gate, profile_gate]);

        let assignment = format!(
            "You are a focused coding subagent working in one prepared, no-egress terminal. All \
             terminal tools are pinned to that session. Implement this goal and its acceptance \
             criteria:\n\n{goal}\n\nThe parent already acquired the work, created the PR, and staged every \
             repository. Do not clone, fetch, publish, or access anything outside the prepared \
             terminal. For PR/user-story information and updates, use only `upstream`: \
             {upstream_description} Every call and output is reviewed by a Boa policy that sees \
             the full tool parameters; a denial is a boundary, not something to bypass. Inspect \
             the final diff and run relevant verification. Then call `finish_work` exactly once \
             with goal analysis and concrete evidence. Set goal_met=true only when the evidence \
             satisfies the goal. Do not claim a push: publication happens parent-side only after \
             that verdict. Return a concise final summary after `finish_work`."
        );
        let system = profile.as_ref().map_or_else(
            || assignment.clone(),
            |profile| {
                format!(
                    "{}\n\n# Constrained terminal assignment\n\n{assignment}",
                    profile.system
                )
            },
        );
        let agent_config = AgentConfig {
            cancel: cancel.clone(),
            cost_limit: profile.as_ref().and_then(|profile| profile.cost_limit),
            ..AgentConfig::default()
        };
        let allowed_tools = profile.as_ref().and_then(|profile| {
            profile.allowed_tools.as_ref().map(|tools| {
                let mut tools = tools.clone();
                for required in [TERMINAL_UPSTREAM_NAME, TERMINAL_FINISH_NAME] {
                    if !tools.iter().any(|tool| tool == required) {
                        tools.push(required.to_string());
                    }
                }
                tools
            })
        });
        let outcome = run_agent(
            &self.client,
            ChatRequest::new(
                model,
                vec![ChatMessage::system(system), ChatMessage::user(goal.clone())],
            ),
            &child_registry,
            &child_ctx,
            &agent_config,
            allowed_tools.as_deref(),
        )
        .await
        .map_err(|error| Error::provider(format!("terminal subagent loop failed: {error}")))?;

        // Never hold the sync mutex across the awaited Boa finalizer. The agent
        // loop is complete, so a short clone is sufficient and deterministic.
        let recorded = verdict
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let stopped = outcome.stopped || cancel.is_cancelled();
        let goal_verdict = if stopped {
            GoalVerdict {
                met: false,
                analysis: "The subagent was stopped; its goal is not accepted and publication is disabled."
                    .into(),
                evidence: vec!["The parent requested cancellation before completion.".into()],
                summary: outcome.content.clone(),
            }
        } else {
            recorded.unwrap_or_else(|| GoalVerdict {
                met: false,
                analysis:
                    "The subagent ended without calling finish_work; the goal is not accepted."
                        .into(),
                evidence: vec!["No structured finish_work verdict was recorded.".into()],
                summary: outcome.content.clone(),
            })
        };

        let mut finalize_attempted = false;
        let mut finalize_result = Json::Null;
        let mut finalize_error = None;
        if goal_verdict.met && !cancel.is_cancelled() {
            finalize_attempted = true;
            let input = json!({
                "goal": goal_verdict.as_json(),
                "policy_context": bound_policy_context,
                "session_id": session_id,
                "agent_content": outcome.content,
            });
            let host = Arc::new(TerminalFinalizerHost {
                registry: parent_registry,
                parent_ctx: ctx.clone(),
                cancel: cancel.clone(),
            });
            let runner = ScriptCodeRunner::new().with_js_limits(JsLimits {
                timeout: std::time::Duration::from_secs(60),
                ..JsLimits::default()
            });
            match runner.eval_with_host(&finalize_code, &input, host).await {
                Ok(result) => finalize_result = result,
                Err(error) => finalize_error = Some(error),
            }
        }
        let pushed_upstream = finalizer_reported_push(
            goal_verdict.met,
            &finalize_result,
            finalize_error.as_deref(),
        );

        Ok(json!({
            "session_id": session_id,
            "profile": profile.as_ref().map(|profile| profile.name.as_str()),
            "content": outcome.content,
            "tool_calls": outcome.tool_invocations.len(),
            "stopped": stopped || cancel.is_cancelled(),
            "goal": goal_verdict.as_json(),
            "finalization": {
                "attempted": finalize_attempted,
                "result": finalize_result,
                "error": finalize_error,
                "pushed_upstream": pushed_upstream,
            }
        }))
    }
}

/// Register the terminal tools when an executor backend is configured; the
/// storage-backed `persist_terminal` / `stage_object` additionally require an
/// object store.
pub(crate) fn register_terminal_tools(
    registry: &mut ToolRegistry,
    manager: Arc<TerminalManager>,
    storage: StorageRegistry,
    store: Store,
    client: OpenRouterClient,
    default_model: String,
    subagent_runs: SubagentRunManager,
) {
    if !manager.is_enabled() {
        return;
    }
    registry.register(Arc::new(OpenTerminalTool {
        manager: manager.clone(),
        storage: storage.clone(),
        store: store.clone(),
    }));
    registry.register(Arc::new(TerminalWriteTool {
        manager: manager.clone(),
    }));
    registry.register(Arc::new(TerminalReadTool {
        manager: manager.clone(),
    }));
    registry.register(Arc::new(ListTerminalsTool {
        manager: manager.clone(),
    }));
    registry.register(Arc::new(CloseTerminalTool {
        manager: manager.clone(),
    }));
    // Workdir file tools — read/create/edit files in a session's working
    // directory (`exec:run`, no storage needed).
    registry.register(Arc::new(ReadFileTool {
        manager: manager.clone(),
    }));
    registry.register(Arc::new(CreateFileTool {
        manager: manager.clone(),
    }));
    registry.register(Arc::new(EditFileTool {
        manager: manager.clone(),
    }));
    registry.register(Arc::new(TerminalSubagentTool {
        manager: manager.clone(),
        store: store.clone(),
        client,
        default_model,
        subagent_runs,
        run_cancel: None,
    }));
    // Storage-backed terminal tools — both write into the object store and fail
    // without one, so they're registered only when storage is configured (and both
    // gated on `exec:run`, like the rest of the surface).
    if manager.has_storage() {
        registry.register(Arc::new(PersistTerminalTool {
            manager: manager.clone(),
        }));
        // `stage_object` / `store_object` resolve arbitrary source/destination stores
        // (not just the terminal's bound default handle), so each carries the full
        // registry + DB store.
        registry.register(Arc::new(StageObjectTool {
            manager: manager.clone(),
            storage: storage.clone(),
            store: store.clone(),
        }));
        registry.register(Arc::new(StoreObjectTool {
            manager,
            storage,
            store,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lazy_store() -> Store {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost/catalerum_test")
            .expect("lazy pool");
        Store::new(pool)
    }
    use catalerum_storage::LocalFsBackend;

    struct RecordingTerminalTool {
        name: &'static str,
        seen: Arc<Mutex<Option<Json>>>,
    }

    #[async_trait]
    impl Tool for RecordingTerminalTool {
        fn name(&self) -> &str {
            self.name
        }

        fn description(&self) -> &str {
            "test terminal tool"
        }

        fn parameters_schema(&self) -> Json {
            json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "data": { "type": "string" }
                },
                "required": ["session_id"]
            })
        }

        async fn invoke(&self, args: Json, _ctx: &ToolContext) -> Result<Json> {
            *self.seen.lock().unwrap_or_else(|error| error.into_inner()) = Some(args.clone());
            Ok(args)
        }
    }

    fn recording_terminal_tool(name: &'static str) -> (Arc<dyn Tool>, Arc<Mutex<Option<Json>>>) {
        let seen = Arc::new(Mutex::new(None));
        (
            Arc::new(RecordingTerminalTool {
                name,
                seen: seen.clone(),
            }),
            seen,
        )
    }

    #[test]
    fn terminal_command_probe_uses_completed_final_block_and_whitelist_order() {
        // PTYs echo the probe source first (including both marker literals). The
        // parser must ignore that and use only the final command-output block.
        let output = format!(
            "printf '{COMMAND_PROBE_START}'; echo python java '{COMMAND_PROBE_END}'\r\n\
             {COMMAND_PROBE_START}\r\nsqlite3\r\npython3\r\nnot-a-candidate\r\n\
             {COMMAND_PROBE_END}\r\n$ "
        );
        assert_eq!(
            parse_available_commands(&output),
            vec!["python3", "sqlite3"]
        );
        assert_eq!(TERMINAL_COMMAND_CANDIDATES.len(), 16);
        assert!(parse_available_commands("no completed marker").is_empty());
    }

    #[test]
    fn terminal_subagent_network_policy_requires_explicit_isolation() {
        assert!(container_network_is_isolated("none"));
        assert!(!container_network_is_isolated(" ISOLATED "));
        assert!(!container_network_is_isolated("bridge"));
        assert!(sandbox_network_is_isolated("none"));
        assert!(sandbox_network_is_isolated(" ISOLATED "));
        assert!(!sandbox_network_is_isolated(""));
        assert!(!sandbox_network_is_isolated("egress"));
    }

    #[tokio::test]
    async fn terminal_subagent_accepts_a_named_profile() {
        let store = lazy_store();
        let manager = Arc::new(TerminalManager::new(
            HashMap::new(),
            store.clone(),
            None,
            &ExecConfig::default(),
            None,
            "test-pod".into(),
        ));
        let tool = TerminalSubagentTool {
            manager,
            store,
            client: OpenRouterClient::new("http://localhost:9", ""),
            default_model: "default".into(),
            subagent_runs: SubagentRunManager::default(),
            run_cancel: None,
        };
        let schema = tool.parameters_schema();
        assert_eq!(schema["properties"]["profile"]["type"], "string");
        assert!(!schema["required"]
            .as_array()
            .expect("required array")
            .iter()
            .any(|name| name == "profile"));
    }

    #[tokio::test]
    async fn terminal_subagent_registry_is_exact_and_pins_the_session() {
        let mut parent = ToolRegistry::new();
        let mut write_seen = None;
        for &name in TERMINAL_SUBAGENT_TOOLS {
            let (tool, seen) = recording_terminal_tool(name);
            if name == "terminal_write" {
                write_seen = Some(seen);
            }
            parent.register(tool);
        }
        parent.register(recording_terminal_tool("open_terminal").0);
        parent.register(recording_terminal_tool("store_object").0);
        parent.register(recording_terminal_tool("unrelated_parent_tool").0);
        let upstream = recording_terminal_tool(TERMINAL_UPSTREAM_NAME).0;
        let finish = recording_terminal_tool(TERMINAL_FINISH_NAME).0;
        let session_id = TerminalSessionId::new();

        let restricted =
            restricted_terminal_subagent_registry(&parent, session_id, upstream, finish);
        let mut names = restricted.names().map(str::to_string).collect::<Vec<_>>();
        names.sort();
        let mut expected = TERMINAL_SUBAGENT_TOOLS
            .iter()
            .map(|name| (*name).to_string())
            .chain([
                TERMINAL_UPSTREAM_NAME.to_string(),
                TERMINAL_FINISH_NAME.to_string(),
            ])
            .collect::<Vec<_>>();
        expected.sort();
        assert_eq!(names, expected);
        assert!(!restricted.contains("open_terminal"));
        assert!(!restricted.contains("store_object"));
        assert!(!restricted.contains("unrelated_parent_tool"));

        let write = restricted
            .get("terminal_write")
            .expect("terminal_write wrapper");
        assert!(write
            .parameters_schema()
            .pointer("/properties/session_id")
            .is_none());
        restricted
            .dispatch(
                "terminal_write",
                json!({ "session_id": TerminalSessionId::new(), "data": "cargo test\n" }),
                &ToolContext::default(),
            )
            .await
            .expect("pinned terminal dispatch");
        let seen = write_seen
            .expect("write recorder")
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .expect("recorded write args");
        assert_eq!(seen["session_id"], session_id.to_string());
        assert_eq!(seen["data"], "cargo test\n");
    }

    #[tokio::test]
    async fn finish_work_records_one_evidence_backed_verdict() {
        let slot = Arc::new(Mutex::new(None));
        let tool = FinishTerminalWorkTool {
            verdict: slot.clone(),
        };
        let args = json!({
            "goal_met": true,
            "analysis": "Acceptance criteria pass.",
            "evidence": ["cargo test: green", "diff inspected"],
            "summary": "Implemented the scoped change."
        });
        let result = tool
            .invoke(args.clone(), &ToolContext::default())
            .await
            .expect("first verdict");
        assert_eq!(result["goal"]["met"], true);
        assert_eq!(
            slot.lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_ref()
                .map(|verdict| verdict.evidence.len()),
            Some(2)
        );
        assert!(tool.invoke(args, &ToolContext::default()).await.is_err());
    }

    #[test]
    fn upstream_push_is_reported_only_after_a_met_goal_and_successful_finalizer() {
        assert!(finalizer_reported_push(
            true,
            &json!({ "pushed": true }),
            None
        ));
        assert!(!finalizer_reported_push(
            false,
            &json!({ "pushed": true }),
            None
        ));
        assert!(!finalizer_reported_push(
            true,
            &json!({ "pushed": false }),
            None
        ));
        assert!(!finalizer_reported_push(
            true,
            &json!({ "pushed": true }),
            Some("push failed")
        ));
    }

    #[tokio::test]
    async fn terminal_upstream_dispatches_only_parent_selected_tools() {
        let mut parent = ToolRegistry::new();
        let (update_pr, seen) = recording_terminal_tool("update_pr");
        parent.register(update_pr);
        parent.register(recording_terminal_tool("update_other_pr").0);
        let upstream = TerminalUpstreamTool {
            description: "PR boundary".into(),
            allowed_tools: vec!["update_pr".into()],
            parent_registry: parent,
            parent_ctx: ToolContext::default(),
            cancel: CancellationToken::new(),
        };
        let result = upstream
            .invoke(
                json!({
                    "tool": "update_pr",
                    "arguments": { "pull_request_id": "pr-42", "body": "status" }
                }),
                &ToolContext::default(),
            )
            .await
            .expect("selected parent tool");
        assert_eq!(result["pull_request_id"], "pr-42");
        assert_eq!(
            seen.lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_ref()
                .and_then(|args| args.get("pull_request_id")),
            Some(&json!("pr-42"))
        );
        let denied = upstream
            .invoke(
                json!({ "tool": "update_other_pr", "arguments": {} }),
                &ToolContext::default(),
            )
            .await
            .expect_err("tool outside allow-list");
        assert!(denied.to_string().contains("upstream allow-list"));
    }

    fn db_url() -> Option<String> {
        std::env::var("CATALERUM_TEST_DATABASE_URL")
            .or_else(|_| std::env::var("DATABASE_URL"))
            .ok()
    }

    /// Multi-pod HA attach guard (SOUL §16 M7): driving a session with no live
    /// handle on this pod yields a *precise* error — "owned by another pod" when a
    /// still-active row belongs to a different pod (route there / sessionAffinity),
    /// "no longer live on this pod" for one this pod owned but whose process ended,
    /// and "unknown or closed" for a genuinely absent id. Gated on live Postgres
    /// (it consults the durable row); no backend needed (the guard fires first).
    #[tokio::test]
    async fn attach_to_foreign_pod_session_errors_clearly() {
        let Some(url) = db_url() else {
            eprintln!("skipping attach_to_foreign_pod_session_errors_clearly: set CATALERUM_TEST_DATABASE_URL");
            return;
        };
        let store = Store::connect(&url).await.expect("connect+migrate");
        let ws = store
            .workspaces()
            .create("term-pod", &format!("term-pod-{}", uuid::Uuid::new_v4()))
            .await
            .expect("ws");

        let cfg = ExecConfig {
            enabled: true,
            backend: "local".to_string(),
            ..Default::default()
        };
        // This pod is "pod-A"; no backends — the guard errors before any driver.
        let mgr = TerminalManager::new(
            HashMap::new(),
            store.clone(),
            None,
            &cfg,
            None,
            "pod-A".to_string(),
        );

        // A row a *different* pod owns (never in this manager's live map).
        let mk = |pod: Option<&str>| NewTerminalSession {
            backend: ExecutorKind::Local,
            host_dir: Some("/tmp/pod".into()),
            sync_prefix: None,
            pod_id: pod.map(str::to_string),
        };
        let foreign = store
            .terminal_sessions()
            .create(ws.id, &mk(Some("pod-B")))
            .await
            .expect("foreign row");
        let err = mgr
            .write(ws.id, foreign.id, b"x".to_vec())
            .await
            .expect_err("driving a foreign-pod session must error");
        let msg = err.to_string();
        assert!(
            msg.contains("owned by another pod") && msg.contains("pod-B"),
            "foreign-pod error should name the owner + advise routing, got: {msg}"
        );

        // A row this pod owns but that isn't live (process ended) → distinct message.
        let mine = store
            .terminal_sessions()
            .create(ws.id, &mk(Some("pod-A")))
            .await
            .expect("own row");
        let err = mgr.read(ws.id, mine.id, 0).await.expect_err("not live");
        assert!(
            err.to_string().contains("no longer live on this pod"),
            "own-but-dead session should say so, got: {err}"
        );

        // A genuinely unknown id → the generic message.
        let err = mgr
            .write(ws.id, TerminalSessionId::new(), b"x".to_vec())
            .await
            .expect_err("unknown id");
        assert!(
            err.to_string()
                .contains("unknown or closed terminal session"),
            "got: {err}"
        );
    }

    /// Foreign-pod file ops (SOUL §16 M7): a `session_host_dir`-backed op
    /// (persist / read_file / edit_file) on a session with no live handle on this
    /// pod must NOT blindly use the row's `host_dir` (a path on the *owning* pod) —
    /// that died with a confusing raw I/O error. It now gives the same precise
    /// verdict the attach guard does: "owned by another pod" (route there) for a
    /// foreign row, "no longer live on this pod" for one this pod owned, and the
    /// generic "unknown terminal session" for an absent id. Gated on live Postgres
    /// (it consults the durable row); no backend/storage needed — the guard fires
    /// before any file (or storage) touch.
    #[tokio::test]
    async fn foreign_pod_file_ops_error_clearly() {
        let Some(url) = db_url() else {
            eprintln!(
                "skipping foreign_pod_file_ops_error_clearly: set CATALERUM_TEST_DATABASE_URL"
            );
            return;
        };
        let store = Store::connect(&url).await.expect("connect+migrate");
        let ws = store
            .workspaces()
            .create("term-fpod", &format!("term-fpod-{}", uuid::Uuid::new_v4()))
            .await
            .expect("ws");

        let cfg = ExecConfig {
            enabled: true,
            backend: "local".to_string(),
            ..Default::default()
        };
        // This pod is "pod-A"; no backends/storage — the host-dir guard errors
        // before any file (or storage) is touched.
        let mgr = TerminalManager::new(
            HashMap::new(),
            store.clone(),
            None,
            &cfg,
            None,
            "pod-A".to_string(),
        );

        // A row carries a `host_dir` naming the *owning* pod's fs — the old code
        // would try to read this path here and fail with a raw I/O error.
        let mk = |pod: Option<&str>| NewTerminalSession {
            backend: ExecutorKind::Local,
            host_dir: Some("/tmp/pod-owned-dir".into()),
            sync_prefix: None,
            pod_id: pod.map(str::to_string),
        };

        // A row a *different* pod owns → the foreign-pod verdict, not an I/O error.
        let foreign = store
            .terminal_sessions()
            .create(ws.id, &mk(Some("pod-B")))
            .await
            .expect("foreign row");
        // `read_file` returns a non-`Debug` `ReadFile`, so unwrap the error via
        // `.err()` rather than `.expect_err()` (which would bound the Ok type).
        let err = mgr
            .read_file(ws.id, foreign.id, "notes.txt", None, None)
            .await
            .err()
            .expect("reading a foreign-pod session's file must error precisely");
        let msg = err.to_string();
        assert!(
            msg.contains("owned by another pod")
                && msg.contains("pod-B")
                && msg.contains("session affinity"),
            "foreign-pod file op should name the owner + advise routing, got: {msg}"
        );
        // The edit path is `session_host_dir`-backed too → same precise verdict.
        let err = mgr
            .edit_file(ws.id, foreign.id, "notes.txt", "a", "b", false)
            .await
            .expect_err("editing a foreign-pod session's file must error precisely");
        assert!(
            err.to_string().contains("owned by another pod"),
            "got: {err}"
        );

        // A row this pod owns but with no live handle (process ended) → the distinct
        // "no longer live on this pod" message rather than a raw I/O error.
        let mine = store
            .terminal_sessions()
            .create(ws.id, &mk(Some("pod-A")))
            .await
            .expect("own row");
        let err = mgr
            .read_file(ws.id, mine.id, "notes.txt", None, None)
            .await
            .err()
            .expect("own-but-dead session file op must error");
        assert!(
            err.to_string().contains("no longer live on this pod"),
            "own-but-dead session should say so, got: {err}"
        );

        // A genuinely unknown id (row absent) → the generic message.
        let err = mgr
            .read_file(ws.id, TerminalSessionId::new(), "notes.txt", None, None)
            .await
            .err()
            .expect("unknown id");
        assert!(
            err.to_string().contains("unknown terminal session"),
            "got: {err}"
        );
    }

    #[test]
    fn resolve_in_dir_joins_relative_and_rejects_escapes() {
        let base = Path::new("/work/abc");
        // A normal relative path joins under the workdir.
        assert_eq!(
            resolve_in_dir(base, "src/main.rs").unwrap(),
            base.join("src/main.rs")
        );
        // Interior `.` normalizes away (matches storage's key contract).
        assert_eq!(resolve_in_dir(base, "a/./b").unwrap(), base.join("a/b"));
        // Empty, absolute, and any `.`/`..` traversal are rejected — a file tool
        // can never escape the session's working directory (§18).
        for bad in ["", "   ", "/etc/passwd", "..", "../x", "a/../../b", "./a"] {
            assert!(
                resolve_in_dir(base, bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn in_container_path_joins_relative_and_rejects_escapes() {
        // Joins under the session's in-container workdir (trailing `/` tolerated).
        assert_eq!(
            in_container_path("/work/.ephemeral/abc", "klarna.pdf").unwrap(),
            "/work/.ephemeral/abc/klarna.pdf"
        );
        assert_eq!(
            in_container_path("/work/x/", "a/b.csv").unwrap(),
            "/work/x/a/b.csv"
        );
        // Same containment contract as `resolve_in_dir`, plus no newlines (the
        // path travels as one shell positional).
        for bad in ["", "  ", "/etc/passwd", "..", "../x", "a/../../b", "a\nb"] {
            assert!(
                in_container_path("/work/x", bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn cap_text_bounds_on_a_char_boundary() {
        let (out, trunc) = cap_text("hello", 100);
        assert_eq!(out, "hello");
        assert!(!trunc);
        // Over the cap → truncated, never split mid-codepoint (`é` is 2 bytes).
        let big = "é".repeat(100); // 200 bytes
        let (out, trunc) = cap_text(&big, 51);
        assert!(trunc);
        assert!(out.len() <= 51);
        assert!(big.starts_with(&out));
    }

    /// End-to-end: open an ephemeral local terminal, run a command, read its
    /// output, persist the working dir to object storage, and close. Gated on a
    /// live Postgres (+ a real PTY); skips offline.
    #[tokio::test]
    async fn manager_open_write_read_persist_close() {
        let Some(url) = db_url() else {
            eprintln!(
                "skipping manager_open_write_read_persist_close: \
                 set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
            );
            return;
        };
        let store = Store::connect(&url).await.expect("connect+migrate");
        let ws = store
            .workspaces()
            .create("term-mgr", &format!("term-mgr-{}", uuid::Uuid::new_v4()))
            .await
            .expect("ws");

        let local: Arc<dyn Executor> = Arc::new(catalerum_exec::LocalExecutor::new());
        let mut backends = HashMap::new();
        backends.insert(ExecutorKind::Local, local);

        let store_dir = tempfile::tempdir().unwrap();
        let storage = StorageHandle {
            backend: Arc::new(LocalFsBackend::new(store_dir.path().to_path_buf())),
            store: "default".to_string(),
            connection: "local-storage".to_string(),
            bucket: "default".to_string(),
            namespaced: true,
        };
        let eph = tempfile::tempdir().unwrap();
        let cfg = ExecConfig {
            enabled: true,
            backend: "local".to_string(),
            // Pin a deterministic shell — the dev user's $SHELL may be an
            // interactive zsh/p10k with a first-run wizard.
            shell: "/bin/sh".to_string(),
            ephemeral_root: eph.path().display().to_string(),
            ..Default::default()
        };
        let mgr = TerminalManager::new(
            backends,
            store.clone(),
            Some(storage),
            &cfg,
            None,
            "test-pod".to_string(),
        );

        let session = mgr.open(ws.id, None).await.expect("open");

        // The marker uses arithmetic expansion so it only appears in the command
        // *output* (`MARK42`), never in the PTY's echo of the typed input
        // (`echo MARK$((6*7))`) — otherwise we'd race ahead of execution.
        mgr.write(
            ws.id,
            session.id,
            b"printf alpha > out.txt; echo MARK$((6*7))\n".to_vec(),
        )
        .await
        .expect("write");

        let mut got = String::new();
        for _ in 0..150 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let bytes = mgr.read(ws.id, session.id, 0).await.expect("read");
            got.push_str(&String::from_utf8_lossy(&bytes));
            if got.contains("MARK42") {
                break;
            }
        }
        assert!(
            got.contains("MARK42"),
            "expected command output, got {got:?}"
        );
        assert_eq!(mgr.list(ws.id).await.expect("list").len(), 1);

        let keys = mgr
            .persist(ws.id, session.id, "snap", None)
            .await
            .expect("persist");
        assert!(keys.contains(&"snap/out.txt".to_string()), "keys: {keys:?}");
        let physical = store_dir
            .path()
            .join(ws.id.to_string())
            .join("snap")
            .join("out.txt");
        assert_eq!(
            tokio::fs::read(&physical).await.expect("persisted file"),
            b"alpha"
        );

        mgr.close(ws.id, session.id).await.expect("close");
        assert!(mgr.list(ws.id).await.expect("list2").is_empty());
        assert!(
            mgr.write(ws.id, session.id, b"x".to_vec()).await.is_err(),
            "writing a closed session must error"
        );
    }

    /// `store_object` (workdir → files store) is the exact inverse of
    /// `stage_object` (files store → workdir): a file written into a session's
    /// working directory streams out to the backend at its namespaced key, and
    /// staging that key back into the session reproduces the original bytes. Gated
    /// on a live Postgres (needs a real session row + host dir).
    #[tokio::test]
    async fn manager_store_object_round_trips_with_stage_object() {
        let Some(url) = db_url() else {
            eprintln!(
                "skipping manager_store_object_round_trips_with_stage_object: \
                 set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
            );
            return;
        };
        let store = Store::connect(&url).await.expect("connect+migrate");
        let ws = store
            .workspaces()
            .create(
                "term-store",
                &format!("term-store-{}", uuid::Uuid::new_v4()),
            )
            .await
            .expect("ws");

        let local: Arc<dyn Executor> = Arc::new(catalerum_exec::LocalExecutor::new());
        let mut backends = HashMap::new();
        backends.insert(ExecutorKind::Local, local);

        let store_dir = tempfile::tempdir().unwrap();
        let storage = StorageHandle {
            backend: Arc::new(LocalFsBackend::new(store_dir.path().to_path_buf())),
            store: "default".to_string(),
            connection: "local-storage".to_string(),
            bucket: "default".to_string(),
            namespaced: true,
        };
        let eph = tempfile::tempdir().unwrap();
        let cfg = ExecConfig {
            enabled: true,
            backend: "local".to_string(),
            shell: "/bin/sh".to_string(),
            ephemeral_root: eph.path().display().to_string(),
            ..Default::default()
        };
        let mgr = TerminalManager::new(
            backends,
            store.clone(),
            Some(storage),
            &cfg,
            None,
            "test-pod".to_string(),
        );
        let session = mgr.open(ws.id, None).await.expect("open");

        // A file the "shell" produced in the workdir.
        mgr.write_file(ws.id, session.id, "out/report.txt", "hello world")
            .await
            .expect("write_file");

        // store_object streams it out to the backend at the workspace-namespaced key.
        let backend = LocalFsBackend::new(store_dir.path().to_path_buf());
        let physical = workspace_object_key(ws.id, "artifacts/report.txt");
        let wrote = mgr
            .store_object(ws.id, session.id, &backend, &physical, "out/report.txt")
            .await
            .expect("store_object");
        assert_eq!(wrote, "hello world".len() as u64);
        let on_disk = store_dir
            .path()
            .join(ws.id.to_string())
            .join("artifacts/report.txt");
        assert_eq!(
            tokio::fs::read(&on_disk).await.expect("stored blob"),
            b"hello world"
        );

        // Staging that same key back into the session reproduces the bytes — the two
        // ops are inverses.
        let staged = mgr
            .stage_object(ws.id, session.id, &backend, &physical, "back/report.txt")
            .await
            .expect("stage_object");
        assert_eq!(staged, "hello world".len() as u64);
        let read = mgr
            .read_file(ws.id, session.id, "back/report.txt", None, None)
            .await
            .expect("read_file");
        assert_eq!(read.content, "hello world");

        // A directory source is rejected (not a file).
        assert!(
            mgr.store_object(ws.id, session.id, &backend, &physical, "out")
                .await
                .is_err(),
            "storing a directory must error"
        );

        mgr.close(ws.id, session.id).await.expect("close");
    }

    /// The workdir file tools end-to-end: open an ephemeral local terminal, then
    /// create / read / edit files in its working directory and assert both the
    /// tool results and the on-disk bytes. No object storage needed (these are
    /// `exec:run` workdir ops). Gated on a live Postgres (+ a real PTY).
    #[tokio::test]
    async fn file_tools_create_read_edit() {
        let Some(url) = db_url() else {
            eprintln!(
                "skipping file_tools_create_read_edit: \
                 set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
            );
            return;
        };
        let store = Store::connect(&url).await.expect("connect+migrate");
        let ws = store
            .workspaces()
            .create(
                "term-files",
                &format!("term-files-{}", uuid::Uuid::new_v4()),
            )
            .await
            .expect("ws");

        let local: Arc<dyn Executor> = Arc::new(catalerum_exec::LocalExecutor::new());
        let mut backends = HashMap::new();
        backends.insert(ExecutorKind::Local, local);
        let eph = tempfile::tempdir().unwrap();
        let cfg = ExecConfig {
            enabled: true,
            backend: "local".to_string(),
            shell: "/bin/sh".to_string(),
            ephemeral_root: eph.path().display().to_string(),
            ..Default::default()
        };
        // No storage handle — the file tools don't touch object storage.
        let mgr = TerminalManager::new(
            backends,
            store.clone(),
            None,
            &cfg,
            None,
            "test-pod".to_string(),
        );
        let session = mgr.open(ws.id, None).await.expect("open");
        let host = PathBuf::from(
            session
                .host_dir
                .clone()
                .expect("local backend has a host dir"),
        );

        // create_file writes through (and makes parent dirs); the bytes land on disk.
        let body = "alpha\nbeta\ngamma\n";
        let (n, overwrote) = mgr
            .write_file(ws.id, session.id, "src/app.txt", body)
            .await
            .expect("create");
        assert_eq!(n, body.len() as u64);
        assert!(!overwrote, "a fresh file is not an overwrite");
        assert_eq!(
            tokio::fs::read_to_string(host.join("src/app.txt"))
                .await
                .unwrap(),
            body
        );

        // read_file returns the whole file, then a single-line window.
        let whole = mgr
            .read_file(ws.id, session.id, "src/app.txt", None, None)
            .await
            .expect("read");
        assert_eq!(whole.content, body);
        assert_eq!(whole.total_lines, 3);
        assert!(!whole.truncated);
        let line2 = mgr
            .read_file(ws.id, session.id, "src/app.txt", Some(2), Some(1))
            .await
            .expect("read window");
        assert_eq!(line2.content, "beta");

        // edit_file: a unique replacement succeeds and persists.
        let made = mgr
            .edit_file(ws.id, session.id, "src/app.txt", "beta", "BETA", false)
            .await
            .expect("edit unique");
        assert_eq!(made, 1);
        assert_eq!(
            tokio::fs::read_to_string(host.join("src/app.txt"))
                .await
                .unwrap(),
            "alpha\nBETA\ngamma\n"
        );

        // A non-unique edit is refused without replace_all, accepted with it.
        mgr.write_file(ws.id, session.id, "dup.txt", "x x x")
            .await
            .expect("dup");
        assert!(
            mgr.edit_file(ws.id, session.id, "dup.txt", "x", "y", false)
                .await
                .is_err(),
            "ambiguous edit must error without replace_all"
        );
        let made = mgr
            .edit_file(ws.id, session.id, "dup.txt", "x", "y", true)
            .await
            .expect("edit all");
        assert_eq!(made, 3);
        assert_eq!(
            tokio::fs::read_to_string(host.join("dup.txt"))
                .await
                .unwrap(),
            "y y y"
        );

        // Path traversal can't escape the workdir; a missing file is a clean error.
        assert!(
            mgr.read_file(ws.id, session.id, "../escape", None, None)
                .await
                .is_err(),
            "traversal must be rejected"
        );
        assert!(
            mgr.read_file(ws.id, session.id, "nope.txt", None, None)
                .await
                .is_err(),
            "a missing file errors"
        );

        mgr.close(ws.id, session.id).await.expect("close");
    }

    fn podman_available() -> bool {
        std::process::Command::new("podman")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// The workdir file tools on a **sandboxed** (container-backed) terminal
    /// (SOUL §20): create / read / window / edit ride the workspace sandbox's
    /// copy channel instead of a host dir — the session has no `host_dir`, the
    /// bytes live in the container, and every op stages through a temp file.
    /// Gated on live Postgres + podman; skips offline.
    #[tokio::test]
    async fn sandboxed_file_tools_ride_the_copy_channel() {
        let Some(url) = db_url() else {
            eprintln!(
                "skipping sandboxed_file_tools_ride_the_copy_channel: \
                 set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
            );
            return;
        };
        if !podman_available() {
            eprintln!("skipping sandboxed_file_tools_ride_the_copy_channel: podman not available");
            return;
        }
        let store = Store::connect(&url).await.expect("connect+migrate");
        let ws = store
            .workspaces()
            .create("term-sbx", &format!("term-sbx-{}", uuid::Uuid::new_v4()))
            .await
            .expect("ws");

        let spec = catalerum_exec::SandboxSpec {
            image: Some("docker.io/library/busybox:latest".to_string()),
            ..Default::default()
        };
        let pod_sandbox = catalerum_exec::PodmanSandbox::new("podman", spec.clone());
        let sandbox_mgr = Arc::new(crate::sandbox::WorkspaceSandboxManager::new(
            Arc::new(pod_sandbox.clone()),
            store.clone(),
            ExecutorKind::Container,
            spec,
            std::time::Duration::from_secs(600),
            "test-pod".to_string(),
        ));
        let eph = tempfile::tempdir().unwrap();
        let cfg = ExecConfig {
            enabled: true,
            backend: "container".to_string(),
            shell: "/bin/sh".to_string(),
            ephemeral_root: eph.path().display().to_string(),
            ..Default::default()
        };
        // No per-call backends: every container terminal runs inside the
        // workspace sandbox.
        let mgr = TerminalManager::new(
            HashMap::new(),
            store.clone(),
            None,
            &cfg,
            Some(sandbox_mgr),
            "test-pod".to_string(),
        );
        let session = match mgr.open(ws.id, None).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("skipping sandboxed_file_tools: open failed: {e}");
                return;
            }
        };
        assert!(
            session.host_dir.is_none(),
            "a sandboxed session keeps its files in the container"
        );

        // create_file rides the copy channel (in-container parent dirs made).
        let body = "alpha\nbeta\ngamma\n";
        let (n, overwrote) = mgr
            .write_file(ws.id, session.id, "src/app.txt", body)
            .await
            .expect("create");
        assert_eq!(n, body.len() as u64);
        assert!(!overwrote, "a fresh file is not an overwrite");
        // A second write is detected as an overwrite by the existence probe.
        let (_, overwrote) = mgr
            .write_file(ws.id, session.id, "src/app.txt", body)
            .await
            .expect("overwrite");
        assert!(overwrote);

        // read_file stages the container file out and round-trips the bytes.
        let whole = mgr
            .read_file(ws.id, session.id, "src/app.txt", None, None)
            .await
            .expect("read");
        assert_eq!(whole.content, body);
        assert_eq!(whole.total_lines, 3);
        assert!(!whole.truncated);
        let line2 = mgr
            .read_file(ws.id, session.id, "src/app.txt", Some(2), Some(1))
            .await
            .expect("read window");
        assert_eq!(line2.content, "beta");

        // edit_file stages out, replaces, and copies back — the follow-up read
        // (freshly staged from the container) proves the write-back landed.
        let made = mgr
            .edit_file(ws.id, session.id, "src/app.txt", "beta", "BETA", false)
            .await
            .expect("edit unique");
        assert_eq!(made, 1);
        let after = mgr
            .read_file(ws.id, session.id, "src/app.txt", None, None)
            .await
            .expect("read after edit");
        assert_eq!(after.content, "alpha\nBETA\ngamma\n");

        // The ambiguity contract holds on the sandbox path too.
        mgr.write_file(ws.id, session.id, "dup.txt", "x x x")
            .await
            .expect("dup");
        assert!(
            mgr.edit_file(ws.id, session.id, "dup.txt", "x", "y", false)
                .await
                .is_err(),
            "ambiguous edit must error without replace_all"
        );
        assert_eq!(
            mgr.edit_file(ws.id, session.id, "dup.txt", "x", "y", true)
                .await
                .expect("edit all"),
            3
        );

        // Missing files and traversal are clean errors.
        assert!(mgr
            .read_file(ws.id, session.id, "nope.txt", None, None)
            .await
            .is_err());
        assert!(mgr
            .read_file(ws.id, session.id, "../escape", None, None)
            .await
            .is_err());

        mgr.close(ws.id, session.id).await.expect("close");
        // Tear the workspace container down (its named volume is kept by design).
        use catalerum_exec::WorkspaceSandbox as _;
        pod_sandbox.destroy(ws.id).await.expect("destroy");
    }

    /// The self-exit reaper end-to-end (SOUL §20): an ephemeral local session
    /// whose shell runs `exit` is reaped without an explicit `close` — its live
    /// handle dropped, the DB row marked closed, and the temp dir removed. Each
    /// manager owns a fresh executor + workspace, so this needs no serialization.
    /// Gated on a live Postgres (+ a real PTY); skips offline.
    #[tokio::test]
    async fn reap_closes_a_self_exited_session() {
        let Some(url) = db_url() else {
            eprintln!(
                "skipping reap_closes_a_self_exited_session: \
                 set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
            );
            return;
        };
        let store = Store::connect(&url).await.expect("connect+migrate");
        let ws = store
            .workspaces()
            .create("term-reap", &format!("term-reap-{}", uuid::Uuid::new_v4()))
            .await
            .expect("ws");

        let local: Arc<dyn Executor> = Arc::new(catalerum_exec::LocalExecutor::new());
        let mut backends = HashMap::new();
        backends.insert(ExecutorKind::Local, local);
        let eph = tempfile::tempdir().unwrap();
        let cfg = ExecConfig {
            enabled: true,
            backend: "local".to_string(),
            shell: "/bin/sh".to_string(),
            ephemeral_root: eph.path().display().to_string(),
            ..Default::default()
        };
        let mgr = TerminalManager::new(
            backends,
            store.clone(),
            None,
            &cfg,
            None,
            "test-pod".to_string(),
        );
        let session = mgr.open(ws.id, None).await.expect("open");
        let host = session
            .host_dir
            .clone()
            .expect("local backend has a host dir");
        assert_eq!(mgr.list(ws.id).await.expect("list").len(), 1);

        // The shell exits on its own — no explicit close_terminal.
        mgr.write(ws.id, session.id, b"exit\n".to_vec())
            .await
            .expect("write exit");

        // Poll the reaper until it observes the exit (the PTY exit is async).
        let mut reaped = 0;
        for _ in 0..200 {
            reaped = mgr.reap().await.expect("reap");
            if reaped > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(reaped, 1, "the self-exited session is reaped");

        // The DB row is now closed (list empty) and the live handle is gone.
        assert!(mgr.list(ws.id).await.expect("list2").is_empty());
        assert!(
            mgr.write(ws.id, session.id, b"x".to_vec()).await.is_err(),
            "writing a reaped session must error"
        );
        // The ephemeral temp dir was cleaned up, and a second reap finds nothing.
        assert!(!Path::new(&host).exists(), "ephemeral dir removed on reap");
        assert_eq!(mgr.reap().await.expect("reap2"), 0, "reap is idempotent");
    }

    #[test]
    fn text_reads_reject_invalid_utf8_and_binary_control_bytes() {
        assert_eq!(
            text_file_content(b"hello\nworld".to_vec(), "ok.txt", false).unwrap(),
            "hello\nworld"
        );
        assert!(text_file_content(vec![0xff, 0xfe], "binary.dat", false)
            .unwrap_err()
            .to_string()
            .contains("binary"));
        assert!(
            text_file_content(b"hello\0world".to_vec(), "binary.dat", false)
                .unwrap_err()
                .to_string()
                .contains("control")
        );
        assert!(text_file_content(vec![1, 2, 3], "binary.dat", false)
            .unwrap_err()
            .to_string()
            .contains("control"));
        assert_eq!(
            text_file_content(vec![b'a', 0xe2, 0x82], "large.txt", true).unwrap(),
            "a"
        );
    }

    #[test]
    fn read_file_description_and_media_detection_follow_model_modalities() {
        let text_only = read_file_description("Read text.", &["text".into()]);
        assert!(text_only.contains("Binary files are rejected"));
        assert!(text_only.contains("unavailable"));
        assert!(text_only.contains("llmleaf"));

        let multimodal = read_file_description("Read text.", &["text".into(), "image".into()]);
        assert!(multimodal.contains("image input"));
        assert!(multimodal.contains("llmleaf"));
        assert!(multimodal.contains("ingested natively"));

        assert_eq!(
            supported_media_path("frame.png", &["text".into(), "image".into()]),
            Some("image/png")
        );
        assert_eq!(
            supported_media_path("clip.mp4", &["text".into(), "image".into()]),
            None
        );
    }
}
