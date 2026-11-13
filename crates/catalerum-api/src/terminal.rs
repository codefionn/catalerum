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
}

mod file_tools;
mod files;
mod fs;
mod session_tools;

pub use fs::{ReadFile, ReadMediaFile};
pub(crate) use file_tools::{stage_object_via, store_object_via};
use file_tools::*;
use fs::*;
use session_tools::*;

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
mod tests;
