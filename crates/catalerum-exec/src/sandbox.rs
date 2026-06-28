//! Per-workspace sandbox backends (SOUL §20).
//!
//! Where the [`Executor`](catalerum_core::provider::Executor) backends spin a
//! **fresh** container/Pod per command or session, a [`WorkspaceSandbox`] keeps
//! exactly **one long-lived, secure sandbox per workspace** and runs every
//! terminal session and `run_command` for that workspace *inside* it (like
//! `docker exec`), sharing a persistent `/work` volume. This is the operator
//! posture: the sandbox's lifecycle is keyed to the workspace, not the call.
//!
//! Two backends sit behind the trait:
//! - [`PodmanSandbox`] — manages a long-lived container per workspace directly
//!   via `podman`/`docker`.
//! - `K8sSandbox` (later slice) — declares a `WorkspaceSandbox` custom resource
//!   and lets the in-cluster `catalerum-operator` reconcile the real Pod.
//!
//! Session I/O reuses the shared [`SessionStore`] PTY machinery, so the
//! agent-facing behavior is identical to the per-session backends; the only
//! divergence is that [`close_session`](WorkspaceSandbox::close_session) tears
//! down the PTY **only** and never the workspace container.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use catalerum_core::error::{Error, Result};
use catalerum_core::model::ExecutorKind;
use catalerum_core::provider::{
    ByteStream, CommandResult, CommandSpec, ResourceLimits, Session, SessionSpec,
};
use catalerum_core::WorkspaceId;
use serde_json::json;
use tokio::io::AsyncWriteExt;

use crate::pty::SessionStore;

/// Mount point inside the sandbox for the workspace's persistent `/work` volume.
const WORKDIR: &str = "/work";
/// Default wall-clock timeout for one-shot [`run`](WorkspaceSandbox::run).
const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// Deterministic, DNS-1123-safe sandbox name for a workspace (the `WorkspaceId`
/// `Display` is lowercase hex + dashes, so this is valid as a container name and
/// a k8s namespace/Pod prefix).
#[must_use]
pub fn sandbox_name(workspace_id: WorkspaceId) -> String {
    format!("catalerum-ws-{workspace_id}")
}

/// Deterministic name of a workspace's persistent `/work` volume.
#[must_use]
pub fn volume_name(workspace_id: WorkspaceId) -> String {
    format!("catalerum-ws-{workspace_id}-work")
}

/// Desired shape of a workspace sandbox. `image`/`volume_size` empty → the
/// backend default; `limits.network` empty/None → full network (the default
/// bridge), the per-workspace posture (cf. the per-session backend's `none`).
#[derive(Clone, Debug, Default)]
pub struct SandboxSpec {
    /// Container image; `None` → the backend's configured default.
    pub image: Option<String>,
    /// CPU / memory / network limits (provider-interpreted).
    pub limits: ResourceLimits,
    /// Persistent `/work` volume size (k8s PVC size, e.g. `10Gi`); ignored by the
    /// podman backend (named volumes are unsized).
    pub volume_size: Option<String>,
}

/// A handle to a workspace's live sandbox.
#[derive(Clone, Debug)]
pub struct SandboxHandle {
    pub workspace_id: WorkspaceId,
    pub backend: ExecutorKind,
    /// Backend reference (container name / Pod name).
    pub reference: String,
    /// Persistent `/work` volume / PVC name, when the backend exposes one.
    pub volume: Option<String>,
    /// The resolved image the sandbox runs.
    pub image: String,
}

/// Lifecycle phase of a workspace sandbox.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SandboxPhase {
    /// No sandbox exists yet (or it's being provisioned).
    Pending,
    /// Live and ready to exec into.
    Ready,
    /// Provisioning failed.
    Failed,
    /// Created but not running (stopped/suspended).
    Stopped,
}

/// Observed status of a workspace sandbox.
#[derive(Clone, Debug)]
pub struct SandboxStatus {
    pub phase: SandboxPhase,
    /// Backend reference (container/Pod name) when one exists.
    pub reference: Option<String>,
    /// The image the sandbox runs, when known.
    pub image: Option<String>,
}

/// One long-lived, secure sandbox **per workspace** (SOUL §20). Terminal
/// sessions and `run_command` `exec` into it; the sandbox itself outlives any
/// single call and is reaped only when idle (or on explicit
/// [`destroy`](WorkspaceSandbox::destroy)).
#[async_trait]
pub trait WorkspaceSandbox: Send + Sync {
    /// Ensure the workspace's sandbox exists and is running, creating (or
    /// adopting) it as needed. Idempotent.
    async fn ensure(&self, workspace_id: WorkspaceId, spec: &SandboxSpec) -> Result<SandboxHandle>;

    /// Open an interactive PTY session inside the workspace sandbox. `spec.cwd`
    /// (if set) is a path **inside** the sandbox (e.g. `/work/<name>`).
    async fn exec_session(&self, workspace_id: WorkspaceId, spec: SessionSpec) -> Result<Session>;

    /// Run a one-shot command/code spec inside the workspace sandbox.
    async fn run(&self, workspace_id: WorkspaceId, cmd: CommandSpec) -> Result<CommandResult>;

    /// The observed status of a workspace's sandbox.
    async fn status(&self, workspace_id: WorkspaceId) -> Result<SandboxStatus>;

    /// Tear down a workspace's sandbox (its persistent volume is kept).
    async fn destroy(&self, workspace_id: WorkspaceId) -> Result<()>;

    /// Write bytes to a session's PTY input.
    async fn session_write(&self, session: &Session, data: Vec<u8>) -> Result<()>;
    /// Drain up to `max_bytes` (0 = all) of a session's buffered output.
    async fn session_read(&self, session: &Session, max_bytes: usize) -> Result<Vec<u8>>;
    /// Subscribe to a session's live output stream.
    async fn session_output(&self, session: &Session) -> Result<ByteStream>;
    /// Resize a session's PTY.
    async fn session_resize(&self, session: &Session, cols: u16, rows: u16) -> Result<()>;
    /// Close a session: kill its PTY **only** — never the workspace container.
    async fn close_session(&self, session: &Session) -> Result<()>;

    /// Reap sessions whose PTY exited on its own (the user ran `exit`, the shell
    /// crashed); the workspace container is left running. Returns the reaped
    /// session ids so the caller can close their durable rows.
    async fn reap(&self) -> Result<Vec<String>>;

    /// Refresh a workspace sandbox's idle clock (best-effort). Backends that GC
    /// on an *external* clock override this to push activity while a session is
    /// attached but quiet — for k8s the operator idle-suspends the Pod purely on
    /// `status.lastActivity`, so a live terminal that isn't running new commands
    /// would otherwise be scaled to 0 out from under the user. The default is a
    /// no-op: podman tracks liveness in-process (its idle reaper skips a sandbox
    /// with open sessions), so it needs no external heartbeat.
    async fn keepalive(&self, _workspace_id: WorkspaceId) -> Result<()> {
        Ok(())
    }

    /// Copy one file from the api host into the sandbox filesystem at the
    /// **absolute** in-sandbox path `dest` (parent dirs created). This is the
    /// channel `stage_object` rides when the backend keeps files inside a
    /// container (no session host dir): download to a host temp file, then
    /// copy in. The caller validates/joins `dest`; implementations pass it as
    /// a shell positional (never spliced into the script), so it needs no
    /// quoting. The default says the backend can't.
    async fn copy_in(&self, _workspace_id: WorkspaceId, _src: &Path, _dest: &str) -> Result<()> {
        Err(Error::invalid(
            "this sandbox backend cannot copy files into the workspace",
        ))
    }

    /// Copy one file **out** of the sandbox (absolute in-sandbox `src`) to the
    /// host path `dest`, returning its byte size — the [`copy_in`] counterpart
    /// that `store_object` rides. The default says the backend can't.
    ///
    /// [`copy_in`]: WorkspaceSandbox::copy_in
    async fn copy_out(&self, _workspace_id: WorkspaceId, _src: &str, _dest: &Path) -> Result<u64> {
        Err(Error::invalid(
            "this sandbox backend cannot copy files out of the workspace",
        ))
    }
}

/// Spawn `cmd args…` and stream the host file `src` into its stdin (the
/// `exec … 'cat > dest'` copy-in). Returns (exit code, stderr). Streams
/// chunk-by-chunk so a large file never buffers whole.
async fn pipe_file_in(cmd: &str, args: &[String], src: &Path) -> Result<(i32, String)> {
    let mut child = tokio::process::Command::new(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        // A cancelled copy (the HTTP request aborted) must not leak the client.
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| Error::provider(format!("`{cmd}` failed to start: {e}")))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| Error::provider("copy-in child has no stdin"))?;
    let mut file = tokio::fs::File::open(src)
        .await
        .map_err(|e| Error::provider(format!("failed to open staged file: {e}")))?;
    let copied = tokio::io::copy(&mut file, &mut stdin).await;
    // Close stdin so the remote `cat` sees EOF even when the copy failed.
    let _ = stdin.shutdown().await;
    drop(stdin);
    let out = child
        .wait_with_output()
        .await
        .map_err(|e| Error::provider(format!("copy-in i/o failed: {e}")))?;
    copied.map_err(|e| Error::provider(format!("copy-in stream failed: {e}")))?;
    Ok((
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

/// Spawn `cmd args…` and stream its stdout into the host file `dest` (the
/// `exec … 'cat src'` copy-out). Returns (exit code, bytes written, stderr).
async fn pipe_file_out(cmd: &str, args: &[String], dest: &Path) -> Result<(i32, u64, String)> {
    let mut child = tokio::process::Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // A cancelled copy (the HTTP request aborted) must not leak the client.
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| Error::provider(format!("`{cmd}` failed to start: {e}")))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::provider("copy-out child has no stdout"))?;
    // Drain stderr concurrently so a chatty child can't deadlock on a full pipe.
    let mut stderr = child.stderr.take();
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(err) = stderr.as_mut() {
            let _ = tokio::io::AsyncReadExt::read_to_end(err, &mut buf).await;
        }
        buf
    });
    let mut file = tokio::fs::File::create(dest)
        .await
        .map_err(|e| Error::provider(format!("failed to create staging file: {e}")))?;
    let copied = tokio::io::copy(&mut stdout, &mut file).await;
    file.flush()
        .await
        .map_err(|e| Error::provider(format!("copy-out flush failed: {e}")))?;
    let status = child
        .wait()
        .await
        .map_err(|e| Error::provider(format!("copy-out wait failed: {e}")))?;
    let stderr_buf = stderr_task.await.unwrap_or_default();
    let bytes = copied.map_err(|e| Error::provider(format!("copy-out stream failed: {e}")))?;
    Ok((
        status.code().unwrap_or(-1),
        bytes,
        String::from_utf8_lossy(&stderr_buf).into_owned(),
    ))
}

/// The in-sandbox script halves of copy-in/copy-out. The target path arrives as
/// `$1` (a positional set by the trailing `sh <dest>` argv), never spliced into
/// the script — no quoting/injection surface.
const COPY_IN_SCRIPT: &str = r#"mkdir -p -- "$(dirname -- "$1")" && cat > "$1""#;
const COPY_OUT_SCRIPT: &str = r#"cat -- "$1""#;

/// Running-state of a named container.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ContainerState {
    Running,
    Stopped,
    Absent,
}

/// Per-workspace container sandbox driven by `podman`/`docker` (SOUL §20). One
/// long-lived `catalerum-ws-<id>` container with the persistent `/work` volume
/// `catalerum-ws-<id>-work` mounted; sessions and `run` are `exec` into it.
/// Cloneable (shares the session/container registries).
#[derive(Clone)]
pub struct PodmanSandbox {
    binary: String,
    default_spec: SandboxSpec,
    default_image: String,
    sessions: SessionStore,
    /// `workspace → resolved image` for workspaces we've ensured this process
    /// (a small cache for `status`; the running-state itself is inspected live).
    images: Arc<Mutex<HashMap<WorkspaceId, String>>>,
    /// `workspace → ensure gate` (serializes concurrent ensures for one ws).
    gates: Arc<Mutex<HashMap<WorkspaceId, Arc<tokio::sync::Mutex<()>>>>>,
    /// `session id → workspace` (so `reap` can drop the right mapping).
    session_ws: Arc<Mutex<HashMap<String, WorkspaceId>>>,
}

impl std::fmt::Debug for PodmanSandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PodmanSandbox")
            .field("binary", &self.binary)
            .field("default_image", &self.default_image)
            .finish_non_exhaustive()
    }
}

impl PodmanSandbox {
    /// A podman/docker per-workspace sandbox driving `binary`, with a default
    /// `spec` (image/limits) applied when a workspace has no override.
    #[must_use]
    pub fn new(binary: impl Into<String>, spec: SandboxSpec) -> Self {
        let binary = binary.into();
        let default_image = spec
            .image
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "docker.io/library/debian:stable-slim".to_string());
        Self {
            binary,
            default_spec: spec,
            default_image,
            sessions: SessionStore::new(),
            images: Arc::new(Mutex::new(HashMap::new())),
            gates: Arc::new(Mutex::new(HashMap::new())),
            session_ws: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn cli(&self) -> &str {
        let b = self.binary.trim();
        if b.is_empty() {
            "podman"
        } else {
            b
        }
    }

    /// Run a CLI subcommand to completion, capturing exit code + stdout/stderr.
    /// Capped + kill-on-drop (see [`crate::proc`]): a runaway command can't OOM
    /// the worker and a timed-out call reaps the CLI client instead of leaking it.
    async fn capture(&self, args: &[String]) -> Result<(i32, String, String)> {
        crate::proc::capture_capped(self.cli(), args, None).await
    }

    /// Pull `image` if it isn't present locally (self-heal).
    async fn ensure_image(&self, image: &str) -> Result<()> {
        let present = tokio::process::Command::new(self.cli())
            .args(["image", "inspect", image])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false);
        if present {
            return Ok(());
        }
        let (code, _out, err) = self.capture(&["pull".into(), image.into()]).await?;
        if code != 0 {
            return Err(Error::provider(format!(
                "failed to pull image `{image}`: {err}"
            )));
        }
        Ok(())
    }

    /// The acquired-or-created ensure gate for a workspace (poison-tolerant).
    fn gate_for(&self, ws: WorkspaceId) -> Arc<tokio::sync::Mutex<()>> {
        let mut gates = self.gates.lock().unwrap_or_else(|e| e.into_inner());
        gates
            .entry(ws)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    fn remember_image(&self, ws: WorkspaceId, image: &str) {
        self.images
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(ws, image.to_string());
    }

    /// The running-state of a named container.
    async fn container_state(&self, name: &str) -> Result<ContainerState> {
        let (code, out, _err) = self
            .capture(&[
                "inspect".into(),
                "-f".into(),
                "{{.State.Running}}".into(),
                name.into(),
            ])
            .await?;
        if code != 0 {
            Ok(ContainerState::Absent)
        } else if out.trim() == "true" {
            Ok(ContainerState::Running)
        } else {
            Ok(ContainerState::Stopped)
        }
    }

    /// Create the keep-alive workspace container with `/work` mounted + hardened.
    async fn create(
        &self,
        name: &str,
        volume: &str,
        image: &str,
        spec: &SandboxSpec,
    ) -> Result<()> {
        self.ensure_image(image).await?;
        let mut args = vec![
            "run".into(),
            "-d".into(),
            "--name".into(),
            name.into(),
            "-v".into(),
            format!("{volume}:{WORKDIR}"),
            "-w".into(),
            WORKDIR.into(),
        ];
        // Full network is the default (no `--network`); an explicit policy narrows it.
        if let Some(net) = spec
            .limits
            .network
            .as_deref()
            .filter(|n| !n.trim().is_empty())
        {
            args.push("--network".into());
            args.push(net.into());
        }
        if let Some(cpu) = spec.limits.cpu {
            args.push("--cpus".into());
            args.push(cpu.to_string());
        }
        if let Some(mem) = spec.limits.memory_mb {
            args.push("--memory".into());
            args.push(format!("{mem}m"));
        }
        args.push("--cap-drop".into());
        args.push("ALL".into());
        args.push("--security-opt".into());
        args.push("no-new-privileges".into());
        args.push(image.into());
        // Portable keep-alive (busybox `sleep` rejects `infinity`).
        args.push("tail".into());
        args.push("-f".into());
        args.push("/dev/null".into());

        let (code, _out, err) = self.capture(&args).await?;
        if code != 0 {
            // A concurrent ensure (or a leftover from a crashed run we didn't
            // observe) may already hold the name — adopt it rather than failing.
            if err.contains("already in use") || err.contains("already exists") {
                return Ok(());
            }
            return Err(Error::provider(format!(
                "failed to start workspace sandbox `{name}`: {err}"
            )));
        }
        Ok(())
    }

    /// Ensure the workspace container exists and is running, then return a handle.
    async fn ensure_inner(&self, ws: WorkspaceId, spec: &SandboxSpec) -> Result<SandboxHandle> {
        let name = sandbox_name(ws);
        let volume = volume_name(ws);
        let image = spec
            .image
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| self.default_image.clone());

        let gate = self.gate_for(ws);
        let _guard = gate.lock().await;

        match self.container_state(&name).await? {
            ContainerState::Running => {}
            ContainerState::Stopped => {
                let _ = self
                    .capture(&["rm".into(), "-f".into(), name.clone()])
                    .await;
                self.create(&name, &volume, &image, spec).await?;
            }
            ContainerState::Absent => self.create(&name, &volume, &image, spec).await?,
        }

        self.remember_image(ws, &image);
        Ok(SandboxHandle {
            workspace_id: ws,
            backend: ExecutorKind::Container,
            reference: name,
            volume: Some(volume),
            image,
        })
    }
}

#[async_trait]
impl WorkspaceSandbox for PodmanSandbox {
    async fn ensure(&self, workspace_id: WorkspaceId, spec: &SandboxSpec) -> Result<SandboxHandle> {
        self.ensure_inner(workspace_id, spec).await
    }

    async fn exec_session(&self, workspace_id: WorkspaceId, spec: SessionSpec) -> Result<Session> {
        self.ensure_inner(workspace_id, &self.default_spec).await?;
        let name = sandbox_name(workspace_id);
        let cwd = spec
            .cwd
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| WORKDIR.to_string());
        // The subdir may not exist yet (a fresh ephemeral session dir) — create it.
        let _ = self
            .capture(&[
                "exec".into(),
                name.clone(),
                "mkdir".into(),
                "-p".into(),
                cwd.clone(),
            ])
            .await;
        let shell = spec
            .shell
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "/bin/sh".to_string());

        // PTY-wrap `<cli> exec -it -w <cwd> <name> <shell…>`. Files live in the
        // shared volume → the session has no host_dir (pty_spec.cwd is None);
        // `cwd` records the in-container workdir for the copy channel instead.
        // The shell may carry args (`bash --noprofile`) — split it, or the
        // runtime would exec a program literally named "bash --noprofile".
        let (shell_prog, shell_args) = crate::pty::split_command(&shell);
        let mut exec_args = vec![
            "exec".into(),
            "-it".into(),
            "-w".into(),
            cwd.clone(),
            name,
            shell_prog,
        ];
        exec_args.extend(shell_args);
        let pty_spec = SessionSpec {
            cols: spec.cols,
            rows: spec.rows,
            ..Default::default()
        };
        let mut session = self
            .sessions
            .open(self.cli(), &exec_args, &pty_spec, false)?;
        session.cwd = Some(cwd);
        self.session_ws
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session.id.clone(), workspace_id);
        Ok(session)
    }

    async fn copy_in(&self, workspace_id: WorkspaceId, src: &Path, dest: &str) -> Result<()> {
        self.ensure_inner(workspace_id, &self.default_spec).await?;
        let name = sandbox_name(workspace_id);
        let args = vec![
            "exec".into(),
            "-i".into(),
            name.clone(),
            "sh".into(),
            "-c".into(),
            COPY_IN_SCRIPT.into(),
            "sh".into(),
            dest.into(),
        ];
        let (code, err) = pipe_file_in(self.cli(), &args, src).await?;
        if code != 0 {
            return Err(Error::provider(format!(
                "failed to copy the file into workspace sandbox `{name}`: {err}"
            )));
        }
        Ok(())
    }

    async fn copy_out(&self, workspace_id: WorkspaceId, src: &str, dest: &Path) -> Result<u64> {
        self.ensure_inner(workspace_id, &self.default_spec).await?;
        let name = sandbox_name(workspace_id);
        let args = vec![
            "exec".into(),
            name,
            "sh".into(),
            "-c".into(),
            COPY_OUT_SCRIPT.into(),
            "sh".into(),
            src.into(),
        ];
        let (code, bytes, err) = pipe_file_out(self.cli(), &args, dest).await?;
        if code != 0 {
            return Err(Error::invalid(format!(
                "failed to copy `{src}` out of the workspace sandbox: {err}"
            )));
        }
        Ok(bytes)
    }

    async fn run(&self, workspace_id: WorkspaceId, cmd: CommandSpec) -> Result<CommandResult> {
        if cmd.argv.is_empty() && cmd.code.is_none() {
            return Err(Error::invalid("run requires a non-empty argv (or code)"));
        }
        self.ensure_inner(workspace_id, &self.default_spec).await?;
        let name = sandbox_name(workspace_id);

        let mut args = vec!["exec".into()];
        if cmd.stdin.is_some() {
            // Keep stdin open so the piped input reaches the command.
            args.push("-i".into());
        }
        let cwd = cmd
            .cwd
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| WORKDIR.to_string());
        args.push("-w".into());
        args.push(cwd);
        for (k, v) in &cmd.env {
            args.push("-e".into());
            args.push(format!("{k}={v}"));
        }
        args.push(name);
        if let Some(code) = &cmd.code {
            let interp = match cmd.language.as_deref() {
                Some("python" | "python3") => "python3",
                _ => "sh",
            };
            args.push(interp.into());
            args.push("-c".into());
            args.push(code.clone());
        } else {
            args.extend(cmd.argv.iter().cloned());
        }

        let timeout = Duration::from_secs(cmd.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
        let collect = crate::proc::capture_capped(self.cli(), &args, cmd.stdin.clone());
        match tokio::time::timeout(timeout, collect).await {
            Ok(res) => {
                let (exit_code, stdout, stderr) = res?;
                Ok(CommandResult {
                    exit_code,
                    stdout,
                    stderr,
                    timed_out: false,
                })
            }
            // Timeout: the exec client is killed on drop. The exec'd process may
            // keep running inside the shared workspace container — we can't kill
            // it without killing the sandbox other sessions use, so it's left to
            // finish (or the workspace's own lifecycle).
            Err(_) => Ok(CommandResult {
                exit_code: -1,
                stdout: String::new(),
                stderr: format!("command timed out after {}s", timeout.as_secs()),
                timed_out: true,
            }),
        }
    }

    async fn status(&self, workspace_id: WorkspaceId) -> Result<SandboxStatus> {
        let name = sandbox_name(workspace_id);
        let phase = match self.container_state(&name).await? {
            ContainerState::Running => SandboxPhase::Ready,
            ContainerState::Stopped => SandboxPhase::Stopped,
            ContainerState::Absent => SandboxPhase::Pending,
        };
        let reference = (phase != SandboxPhase::Pending).then(|| name.clone());
        let image = self
            .images
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&workspace_id)
            .cloned();
        Ok(SandboxStatus {
            phase,
            reference,
            image,
        })
    }

    async fn destroy(&self, workspace_id: WorkspaceId) -> Result<()> {
        let name = sandbox_name(workspace_id);
        // Remove the container; keep the named volume so `/work` persists.
        let _ = self.capture(&["rm".into(), "-f".into(), name]).await;
        self.images
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&workspace_id);
        Ok(())
    }

    async fn session_write(&self, session: &Session, data: Vec<u8>) -> Result<()> {
        self.sessions.write(&session.id, data).await
    }

    async fn session_read(&self, session: &Session, max_bytes: usize) -> Result<Vec<u8>> {
        self.sessions.read(&session.id, max_bytes)
    }

    async fn session_output(&self, session: &Session) -> Result<ByteStream> {
        self.sessions.output(&session.id)
    }

    async fn session_resize(&self, session: &Session, cols: u16, rows: u16) -> Result<()> {
        self.sessions.resize(&session.id, cols, rows)
    }

    async fn close_session(&self, session: &Session) -> Result<()> {
        // Kill the PTY only — the shared workspace container stays running.
        let _ = self.sessions.close(&session.id).await;
        self.session_ws
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&session.id);
        Ok(())
    }

    async fn reap(&self) -> Result<Vec<String>> {
        let dead = self.sessions.reap_exited()?;
        if !dead.is_empty() {
            let mut map = self.session_ws.lock().unwrap_or_else(|e| e.into_inner());
            for id in &dead {
                map.remove(id);
            }
        }
        Ok(dead)
    }
}

/// How long to wait for a `WorkspaceSandbox` CR to reach `Ready`.
const K8S_READY_TIMEOUT_SECS: u64 = 180;
/// Management namespace the `WorkspaceSandbox` CRs live in.
const K8S_MANAGEMENT_NAMESPACE: &str = "catalerum-system";

/// Per-workspace sandbox driven via `kubectl` against the **catalerum-operator**
/// (SOUL §20). Instead of managing Pods directly, it declares a `WorkspaceSandbox`
/// custom resource (`catalerum.dev/v1alpha1`) and waits for the operator to
/// reconcile it `Ready`, then `exec`s into the operator-managed Pod. Idle GC,
/// hardening, NetworkPolicy, and the PVC are all the operator's job. Requires
/// `kubectl` + the CRD/operator installed (see `deploy/`).
#[derive(Clone)]
pub struct K8sSandbox {
    kubectl: String,
    management_namespace: String,
    default_spec: SandboxSpec,
    default_image: String,
    idle_ttl_seconds: u64,
    sessions: SessionStore,
    /// `workspace → (pod namespace, pod name)` once the CR is `Ready`.
    pods: Arc<Mutex<HashMap<WorkspaceId, (String, String)>>>,
    session_ws: Arc<Mutex<HashMap<String, WorkspaceId>>>,
    gates: Arc<Mutex<HashMap<WorkspaceId, Arc<tokio::sync::Mutex<()>>>>>,
}

impl std::fmt::Debug for K8sSandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("K8sSandbox")
            .field("kubectl", &self.kubectl)
            .field("management_namespace", &self.management_namespace)
            .finish_non_exhaustive()
    }
}

impl K8sSandbox {
    /// A k8s per-workspace sandbox driving `kubectl`, declaring CRs in the
    /// operator's management namespace. `idle_ttl_seconds` is written to the CR so
    /// the operator suspends an idle sandbox (`0` → never).
    #[must_use]
    pub fn new(kubectl: impl Into<String>, spec: SandboxSpec, idle_ttl_seconds: u64) -> Self {
        let default_image = spec
            .image
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "docker.io/library/debian:stable-slim".to_string());
        Self {
            kubectl: kubectl.into(),
            management_namespace: K8S_MANAGEMENT_NAMESPACE.to_string(),
            default_spec: spec,
            default_image,
            idle_ttl_seconds,
            sessions: SessionStore::new(),
            pods: Arc::new(Mutex::new(HashMap::new())),
            session_ws: Arc::new(Mutex::new(HashMap::new())),
            gates: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn cli(&self) -> &str {
        let k = self.kubectl.trim();
        if k.is_empty() {
            "kubectl"
        } else {
            k
        }
    }

    /// The `WorkspaceSandbox` CR name for a workspace.
    fn cr_name(ws: WorkspaceId) -> String {
        format!("catalerum-ws-{ws}")
    }

    /// `Full` unless the network policy is `none`/`isolated`.
    fn net_mode(spec: &SandboxSpec) -> &'static str {
        match spec
            .limits
            .network
            .as_deref()
            .map(|s| s.trim().to_ascii_lowercase())
        {
            Some(n) if n == "none" || n == "isolated" => "Isolated",
            _ => "Full",
        }
    }

    fn gate_for(&self, ws: WorkspaceId) -> Arc<tokio::sync::Mutex<()>> {
        let mut gates = self.gates.lock().unwrap_or_else(|e| e.into_inner());
        gates
            .entry(ws)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    fn pod_for(&self, ws: WorkspaceId) -> Result<(String, String)> {
        self.pods
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&ws)
            .cloned()
            .ok_or_else(|| Error::provider("workspace sandbox pod is not ready"))
    }

    /// Run a `kubectl` subcommand to completion. Capped + kill-on-drop (see
    /// [`crate::proc`]).
    async fn capture(&self, args: &[String]) -> Result<(i32, String, String)> {
        crate::proc::capture_capped(self.cli(), args, None).await
    }

    /// Run a `kubectl` subcommand feeding `input` on stdin (for `apply -f -`).
    async fn capture_stdin(&self, args: &[String], input: &str) -> Result<(i32, String, String)> {
        crate::proc::capture_capped(self.cli(), args, Some(input.to_string())).await
    }

    /// The `WorkspaceSandbox` CR manifest for a workspace.
    fn manifest(&self, ws: WorkspaceId, spec: &SandboxSpec) -> serde_json::Value {
        let image = spec
            .image
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| self.default_image.clone());
        let mut cr_spec = json!({
            "workspaceId": ws.to_string(),
            "workVolumeSize": spec.volume_size.clone().unwrap_or_else(|| "10Gi".to_string()),
            "networkPolicy": Self::net_mode(spec),
            "idleTtlSeconds": self.idle_ttl_seconds,
            "image": image,
        });
        if let Some(cpu) = spec.limits.cpu {
            cr_spec["cpuLimit"] = json!(cpu.to_string());
        }
        if let Some(mem) = spec.limits.memory_mb {
            cr_spec["memoryLimit"] = json!(format!("{mem}Mi"));
        }
        json!({
            "apiVersion": "catalerum.dev/v1alpha1",
            "kind": "WorkspaceSandbox",
            "metadata": { "name": Self::cr_name(ws), "namespace": self.management_namespace },
            "spec": cr_spec,
        })
    }

    /// Patch `status.lastActivity = now` so the operator keeps (or wakes) the
    /// sandbox; drives idle GC. Best-effort.
    async fn patch_activity(&self, cr: &str) {
        let now = chrono::Utc::now().to_rfc3339();
        let patch = format!("{{\"status\":{{\"lastActivity\":\"{now}\"}}}}");
        let _ = self
            .capture(&[
                "patch".into(),
                "workspacesandbox".into(),
                cr.into(),
                "-n".into(),
                self.management_namespace.clone(),
                "--subresource".into(),
                "status".into(),
                "--type".into(),
                "merge".into(),
                "-p".into(),
                patch,
            ])
            .await;
    }

    async fn jsonpath(&self, cr: &str, path: &str) -> Result<String> {
        let (_c, out, _e) = self
            .capture(&[
                "get".into(),
                "workspacesandbox".into(),
                cr.into(),
                "-n".into(),
                self.management_namespace.clone(),
                "-o".into(),
                format!("jsonpath={{{path}}}"),
            ])
            .await?;
        Ok(out.trim().to_string())
    }

    async fn ensure_inner(&self, ws: WorkspaceId, spec: &SandboxSpec) -> Result<SandboxHandle> {
        let gate = self.gate_for(ws);
        let _guard = gate.lock().await;
        let cr = Self::cr_name(ws);

        // Declare (or update) the CR; the operator reconciles the real objects.
        let manifest = serde_json::to_string(&self.manifest(ws, spec))?;
        let (code, _o, err) = self
            .capture_stdin(&["apply".into(), "-f".into(), "-".into()], &manifest)
            .await?;
        if code != 0 {
            return Err(Error::provider(format!(
                "kubectl apply WorkspaceSandbox failed (is the CRD + catalerum-operator installed?): {err}"
            )));
        }
        // Wake/keep-alive, then wait for the operator to mark it Ready.
        self.patch_activity(&cr).await;
        let deadline = Instant::now() + Duration::from_secs(K8S_READY_TIMEOUT_SECS);
        loop {
            match self.jsonpath(&cr, ".status.phase").await?.as_str() {
                "Ready" => break,
                "Failed" => return Err(Error::provider("workspace sandbox failed to provision")),
                _ => {}
            }
            if Instant::now() >= deadline {
                return Err(Error::provider(
                    "workspace sandbox did not become ready in time",
                ));
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        let pod = self.jsonpath(&cr, ".status.podName").await?;
        if pod.is_empty() {
            return Err(Error::provider(
                "workspace sandbox ready but reported no pod",
            ));
        }
        let pod_ns = {
            let ns = self.jsonpath(&cr, ".status.namespace").await?;
            if ns.is_empty() {
                format!("catalerum-ws-{ws}")
            } else {
                ns
            }
        };
        self.pods
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(ws, (pod_ns.clone(), pod.clone()));
        let image = spec
            .image
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| self.default_image.clone());
        Ok(SandboxHandle {
            workspace_id: ws,
            backend: ExecutorKind::Kubernetes,
            reference: pod,
            volume: Some("work".to_string()),
            image,
        })
    }
}

#[async_trait]
impl WorkspaceSandbox for K8sSandbox {
    async fn ensure(&self, workspace_id: WorkspaceId, spec: &SandboxSpec) -> Result<SandboxHandle> {
        self.ensure_inner(workspace_id, spec).await
    }

    async fn keepalive(&self, workspace_id: WorkspaceId) -> Result<()> {
        // Push `status.lastActivity` so the operator keeps (or wakes) the Pod
        // while a terminal is attached. Best-effort — patch_activity swallows.
        self.patch_activity(&Self::cr_name(workspace_id)).await;
        Ok(())
    }

    async fn exec_session(&self, workspace_id: WorkspaceId, spec: SessionSpec) -> Result<Session> {
        self.ensure_inner(workspace_id, &self.default_spec).await?;
        let (pod_ns, pod) = self.pod_for(workspace_id)?;
        let cwd = spec
            .cwd
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| WORKDIR.to_string());
        let shell = spec
            .shell
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "/bin/sh".to_string());
        // `kubectl exec` has no `-w`; cd into the subdir (creating it) then exec.
        // The cwd and shell arrive as *positionals* — never spliced into the
        // script (no quoting/injection surface), matching `run` and the copy
        // scripts. The shell may carry args (`bash --noprofile`) — split it so
        // `exec "$@"` runs the program with its args instead of one literal name.
        let (shell_prog, shell_args) = crate::pty::split_command(&shell);
        let mut exec_args = vec![
            "exec".into(),
            "-it".into(),
            "-n".into(),
            pod_ns,
            pod,
            "--".into(),
            "sh".into(),
            "-c".into(),
            r#"mkdir -p -- "$1" && cd -- "$1" || exit 1; shift; exec "$@""#.into(),
            "sh".into(),
            cwd.clone(),
            shell_prog,
        ];
        exec_args.extend(shell_args);
        let pty_spec = SessionSpec {
            cols: spec.cols,
            rows: spec.rows,
            ..Default::default()
        };
        let mut session = self
            .sessions
            .open(self.cli(), &exec_args, &pty_spec, false)?;
        // Files live in the Pod (no host_dir); record the in-Pod workdir so the
        // copy channel (`stage_object`) can still target this session's files.
        session.cwd = Some(cwd);
        self.session_ws
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session.id.clone(), workspace_id);
        Ok(session)
    }

    async fn copy_in(&self, workspace_id: WorkspaceId, src: &Path, dest: &str) -> Result<()> {
        self.ensure_inner(workspace_id, &self.default_spec).await?;
        let (pod_ns, pod) = self.pod_for(workspace_id)?;
        let args = vec![
            "exec".into(),
            "-i".into(),
            "-n".into(),
            pod_ns,
            pod.clone(),
            "--".into(),
            "sh".into(),
            "-c".into(),
            COPY_IN_SCRIPT.into(),
            "sh".into(),
            dest.into(),
        ];
        let (code, err) = pipe_file_in(self.cli(), &args, src).await?;
        if code != 0 {
            return Err(Error::provider(format!(
                "failed to copy the file into workspace sandbox pod `{pod}`: {err}"
            )));
        }
        Ok(())
    }

    async fn copy_out(&self, workspace_id: WorkspaceId, src: &str, dest: &Path) -> Result<u64> {
        self.ensure_inner(workspace_id, &self.default_spec).await?;
        let (pod_ns, pod) = self.pod_for(workspace_id)?;
        let args = vec![
            "exec".into(),
            "-n".into(),
            pod_ns,
            pod,
            "--".into(),
            "sh".into(),
            "-c".into(),
            COPY_OUT_SCRIPT.into(),
            "sh".into(),
            src.into(),
        ];
        let (code, bytes, err) = pipe_file_out(self.cli(), &args, dest).await?;
        if code != 0 {
            return Err(Error::invalid(format!(
                "failed to copy `{src}` out of the workspace sandbox: {err}"
            )));
        }
        Ok(bytes)
    }

    async fn run(&self, workspace_id: WorkspaceId, cmd: CommandSpec) -> Result<CommandResult> {
        if cmd.argv.is_empty() && cmd.code.is_none() {
            return Err(Error::invalid("run requires a non-empty argv (or code)"));
        }
        self.ensure_inner(workspace_id, &self.default_spec).await?;
        let (pod_ns, pod) = self.pod_for(workspace_id)?;
        let cwd = cmd
            .cwd
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| WORKDIR.to_string());
        // The command (argv, or interpreter + code) is passed as positional args so
        // no shell-quoting of the payload is needed: `sh -c SCRIPT sh <cwd> <argv…>`
        // sets $1=cwd, $2…=argv; we cd then exec the rest.
        let inner_argv: Vec<String> = if let Some(code) = &cmd.code {
            let interp = match cmd.language.as_deref() {
                Some("python" | "python3") => "python3",
                _ => "sh",
            };
            vec![interp.to_string(), "-c".to_string(), code.clone()]
        } else {
            cmd.argv.clone()
        };
        let mut args = vec!["exec".into()];
        if cmd.stdin.is_some() {
            // Keep stdin open so the piped input reaches the command.
            args.push("-i".into());
        }
        args.extend([
            "-n".into(),
            pod_ns,
            pod,
            "--".into(),
            "sh".into(),
            "-c".into(),
            "cd \"$1\" || exit 1; shift; exec \"$@\"".into(),
            "sh".into(),
            cwd,
        ]);
        args.extend(inner_argv);

        let timeout = Duration::from_secs(cmd.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
        let collect = crate::proc::capture_capped(self.cli(), &args, cmd.stdin.clone());
        match tokio::time::timeout(timeout, collect).await {
            Ok(res) => {
                let (exit_code, stdout, stderr) = res?;
                Ok(CommandResult {
                    exit_code,
                    stdout,
                    stderr,
                    timed_out: false,
                })
            }
            // Timeout: the exec client is killed on drop. The exec'd process may
            // keep running inside the shared workspace Pod — we can't kill it
            // without killing the sandbox other sessions use, so it's left to
            // finish (or the operator's idle lifecycle).
            Err(_) => Ok(CommandResult {
                exit_code: -1,
                stdout: String::new(),
                stderr: format!("command timed out after {}s", timeout.as_secs()),
                timed_out: true,
            }),
        }
    }

    async fn status(&self, workspace_id: WorkspaceId) -> Result<SandboxStatus> {
        let cr = Self::cr_name(workspace_id);
        let phase = match self.jsonpath(&cr, ".status.phase").await?.as_str() {
            "Ready" => SandboxPhase::Ready,
            "Failed" => SandboxPhase::Failed,
            "Suspended" => SandboxPhase::Stopped,
            "" => SandboxPhase::Pending,
            _ => SandboxPhase::Pending,
        };
        let pod = self.jsonpath(&cr, ".status.podName").await.ok();
        Ok(SandboxStatus {
            phase,
            reference: pod.filter(|p| !p.is_empty()),
            image: None,
        })
    }

    async fn destroy(&self, workspace_id: WorkspaceId) -> Result<()> {
        let cr = Self::cr_name(workspace_id);
        // The operator GCs the namespace/Pod/PVC when the CR (with our finalizer) is removed.
        let _ = self
            .capture(&[
                "delete".into(),
                "workspacesandbox".into(),
                cr,
                "-n".into(),
                self.management_namespace.clone(),
                "--ignore-not-found".into(),
            ])
            .await;
        self.pods
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&workspace_id);
        Ok(())
    }

    async fn session_write(&self, session: &Session, data: Vec<u8>) -> Result<()> {
        self.sessions.write(&session.id, data).await
    }

    async fn session_read(&self, session: &Session, max_bytes: usize) -> Result<Vec<u8>> {
        self.sessions.read(&session.id, max_bytes)
    }

    async fn session_output(&self, session: &Session) -> Result<ByteStream> {
        self.sessions.output(&session.id)
    }

    async fn session_resize(&self, session: &Session, cols: u16, rows: u16) -> Result<()> {
        self.sessions.resize(&session.id, cols, rows)
    }

    async fn close_session(&self, session: &Session) -> Result<()> {
        let _ = self.sessions.close(&session.id).await;
        self.session_ws
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&session.id);
        Ok(())
    }

    async fn reap(&self) -> Result<Vec<String>> {
        let dead = self.sessions.reap_exited()?;
        if !dead.is_empty() {
            let mut map = self.session_ws.lock().unwrap_or_else(|e| e.into_inner());
            for id in &dead {
                map.remove(id);
            }
        }
        Ok(dead)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws() -> WorkspaceId {
        WorkspaceId::from_uuid(uuid::Uuid::nil())
    }

    #[test]
    fn k8s_manifest_matches_the_crd() {
        let spec = SandboxSpec {
            image: Some("busybox".to_string()),
            limits: ResourceLimits {
                cpu: Some(2),
                memory_mb: Some(512),
                network: Some("none".to_string()),
            },
            volume_size: Some("5Gi".to_string()),
        };
        let sandbox = K8sSandbox::new("kubectl", spec.clone(), 1800);
        let id = ws();
        let m = sandbox.manifest(id, &spec);
        assert_eq!(m["apiVersion"], "catalerum.dev/v1alpha1");
        assert_eq!(m["kind"], "WorkspaceSandbox");
        assert_eq!(
            m["metadata"]["name"],
            "catalerum-ws-00000000-0000-0000-0000-000000000000"
        );
        assert_eq!(m["metadata"]["namespace"], "catalerum-system");
        let s = &m["spec"];
        assert_eq!(s["workspaceId"], id.to_string());
        assert_eq!(s["workVolumeSize"], "5Gi");
        // network "none" → Isolated.
        assert_eq!(s["networkPolicy"], "Isolated");
        assert_eq!(s["idleTtlSeconds"], 1800);
        assert_eq!(s["image"], "busybox");
        assert_eq!(s["cpuLimit"], "2");
        assert_eq!(s["memoryLimit"], "512Mi");
    }

    #[test]
    fn k8s_manifest_defaults_to_full_network() {
        let spec = SandboxSpec {
            volume_size: Some("10Gi".to_string()),
            ..Default::default()
        };
        let sandbox = K8sSandbox::new("kubectl", spec.clone(), 0);
        let m = sandbox.manifest(ws(), &spec);
        assert_eq!(m["spec"]["networkPolicy"], "Full");
        assert_eq!(m["spec"]["idleTtlSeconds"], 0);
    }

    #[test]
    fn names_are_deterministic_and_dns_safe() {
        let id = ws();
        let name = sandbox_name(id);
        assert_eq!(name, "catalerum-ws-00000000-0000-0000-0000-000000000000");
        assert_eq!(volume_name(id), format!("{name}-work"));
        // Container/DNS-1123 charset: lowercase alphanumerics + '-', starts alpha.
        assert!(name.starts_with("catalerum-ws-"));
        assert!(name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'));
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

    /// End-to-end: ensure a workspace sandbox, exec a session into it, run a
    /// one-shot command, open a *second* session into the **same** container, and
    /// destroy. Skips when podman (or the image) is unavailable.
    #[tokio::test]
    async fn workspace_sandbox_shares_one_container() {
        if !podman_available() {
            eprintln!("skipping workspace_sandbox_shares_one_container: podman not available");
            return;
        }
        let spec = SandboxSpec {
            image: Some("docker.io/library/busybox:latest".to_string()),
            ..Default::default()
        };
        let sandbox = PodmanSandbox::new("podman", spec);
        let id = WorkspaceId::new();

        let handle = match sandbox.ensure(id, &sandbox.default_spec).await {
            Ok(h) => h,
            Err(e) => {
                eprintln!("skipping workspace_sandbox: ensure failed: {e}");
                return;
            }
        };
        assert_eq!(handle.reference, sandbox_name(id));

        // A one-shot run writes to the shared volume.
        let out = sandbox
            .run(
                id,
                CommandSpec {
                    argv: vec!["sh".into(), "-c".into(), "echo hi > /work/marker".into()],
                    ..Default::default()
                },
            )
            .await
            .expect("run");
        assert_eq!(out.exit_code, 0, "run stderr: {}", out.stderr);

        // A session execs into the same container and sees that file.
        let session = sandbox
            .exec_session(
                id,
                SessionSpec {
                    shell: Some("/bin/sh".into()),
                    ..Default::default()
                },
            )
            .await
            .expect("session");
        assert!(session.host_dir.is_none(), "files live in the container");
        sandbox
            .session_write(&session, b"cat /work/marker\n".to_vec())
            .await
            .expect("write");
        let mut got = String::new();
        for _ in 0..150 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            got.push_str(&String::from_utf8_lossy(
                &sandbox.session_read(&session, 0).await.expect("read"),
            ));
            if got.contains("hi") {
                break;
            }
        }
        assert!(
            got.contains("hi"),
            "session saw the shared volume, got {got:?}"
        );

        // A second session reuses the same workspace container (one ensure path).
        let s2 = sandbox
            .exec_session(id, SessionSpec::default())
            .await
            .expect("second session");
        assert_ne!(s2.id, session.id);

        sandbox.close_session(&session).await.expect("close 1");
        sandbox.close_session(&s2).await.expect("close 2");
        sandbox.destroy(id).await.expect("destroy");
    }

    /// Round-trip the sandbox copy channel (`stage_object`/`store_object`'s
    /// transport): host file → `copy_in` → verify inside the container →
    /// `copy_out` → identical bytes back on the host. Skips without podman.
    #[tokio::test]
    async fn workspace_sandbox_copies_files_in_and_out() {
        if !podman_available() {
            eprintln!("skipping workspace_sandbox_copies_files_in_and_out: podman not available");
            return;
        }
        let spec = SandboxSpec {
            image: Some("docker.io/library/busybox:latest".to_string()),
            ..Default::default()
        };
        let sandbox = PodmanSandbox::new("podman", spec);
        let id = WorkspaceId::new();
        if let Err(e) = sandbox.ensure(id, &sandbox.default_spec).await {
            eprintln!("skipping workspace_sandbox_copies: ensure failed: {e}");
            return;
        }

        // Binary-ish content (embedded NUL + newline) proves the channel is
        // byte-clean, not line-oriented.
        let payload = b"stage me\x00\nbinary tail".to_vec();
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("in.bin");
        tokio::fs::write(&src, &payload).await.expect("write src");

        // In: parents are created by the copy script.
        sandbox
            .copy_in(id, &src, "/work/sub/dir/in.bin")
            .await
            .expect("copy_in");
        let out = sandbox
            .run(
                id,
                CommandSpec {
                    argv: vec!["wc".into(), "-c".into(), "/work/sub/dir/in.bin".into()],
                    ..Default::default()
                },
            )
            .await
            .expect("run");
        assert_eq!(out.exit_code, 0, "wc stderr: {}", out.stderr);
        assert!(
            out.stdout.trim().starts_with(&payload.len().to_string()),
            "container sees the full byte count, got {:?}",
            out.stdout
        );

        // Out: the same bytes come back.
        let back = dir.path().join("out.bin");
        let n = sandbox
            .copy_out(id, "/work/sub/dir/in.bin", &back)
            .await
            .expect("copy_out");
        assert_eq!(n, payload.len() as u64);
        assert_eq!(tokio::fs::read(&back).await.expect("read back"), payload);

        // A missing source is a clean error, not an empty file.
        assert!(sandbox
            .copy_out(id, "/work/definitely-missing", &back)
            .await
            .is_err());

        sandbox.destroy(id).await.expect("destroy");
    }
}
