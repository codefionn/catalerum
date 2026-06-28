//! The container (podman / docker) executor backend (SOUL §20). The **default
//! sandbox**: a long-lived container per interactive session with the working
//! directory bind-mounted at `/work`, CPU/mem/net limits, and dropped caps; plus
//! one-shot [`run`](Executor::run) in an ephemeral `--rm` container.
//!
//! The interactive PTY is a `<binary> exec -it <container> <shell>` driven
//! through the shared [`SessionStore`] — the same PTY machinery the local
//! backend uses, just wrapping the container-exec process. The host working dir
//! is the bind-mount source, so files written inside `/work` appear on the host
//! and an ephemeral session can still be flushed to object storage.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use catalerum_core::error::{Error, Result};
use catalerum_core::provider::{
    ByteStream, CommandResult, CommandSpec, Executor, Session, SessionSpec,
};

use crate::pty::SessionStore;

/// Mount point inside the container for the session's working directory.
const WORKDIR: &str = "/work";
/// Default wall-clock timeout for one-shot `run`.
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Drives `podman`/`docker` to run interactive container sessions + one-shot
/// commands (SOUL §20). Cloneable (shares the session/container registries).
#[derive(Clone, Debug)]
pub struct ContainerExecutor {
    binary: String,
    image: String,
    network: String,
    sessions: SessionStore,
    /// `session id → container id`, so a session's container is removed on close.
    containers: Arc<Mutex<HashMap<String, String>>>,
}

impl ContainerExecutor {
    /// A container backend driving `binary` (`podman`/`docker`) with a default
    /// `image` and `network` policy (e.g. `none`).
    #[must_use]
    pub fn new(
        binary: impl Into<String>,
        image: impl Into<String>,
        network: impl Into<String>,
    ) -> Self {
        Self {
            binary: binary.into(),
            image: image.into(),
            network: network.into(),
            sessions: SessionStore::new(),
            containers: Arc::new(Mutex::new(HashMap::new())),
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
    /// the worker, and a timed-out call reaps the CLI client instead of leaking it.
    async fn capture(&self, args: &[String]) -> Result<(i32, String, String)> {
        crate::proc::capture_capped(self.cli(), args, None).await
    }

    /// Pull `image` if it isn't present locally (self-heal, cf. `ensure_container`).
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

    /// Append the hardening + resource flags shared by `run` and session start.
    fn shape(
        &self,
        args: &mut Vec<String>,
        cwd: Option<&str>,
        env: &[(String, String)],
        cpu: Option<u32>,
        memory_mb: Option<u64>,
        network: Option<&str>,
    ) {
        if let Some(cwd) = cwd {
            args.push("-v".into());
            args.push(format!("{cwd}:{WORKDIR}"));
            args.push("-w".into());
            args.push(WORKDIR.into());
        }
        let net = network
            .filter(|n| !n.trim().is_empty())
            .unwrap_or(self.network.as_str());
        if !net.trim().is_empty() {
            args.push("--network".into());
            args.push(net.to_string());
        }
        if let Some(cpu) = cpu {
            args.push("--cpus".into());
            args.push(cpu.to_string());
        }
        if let Some(mem) = memory_mb {
            args.push("--memory".into());
            args.push(format!("{mem}m"));
        }
        args.push("--cap-drop".into());
        args.push("ALL".into());
        args.push("--security-opt".into());
        args.push("no-new-privileges".into());
        for (k, v) in env {
            args.push("-e".into());
            args.push(format!("{k}={v}"));
        }
    }
}

#[async_trait]
impl Executor for ContainerExecutor {
    async fn run(&self, cmd: CommandSpec) -> Result<CommandResult> {
        if cmd.argv.is_empty() && cmd.code.is_none() {
            return Err(Error::invalid("run requires a non-empty argv (or code)"));
        }
        self.ensure_image(&self.image).await?;

        // Name the ephemeral container so a timeout can remove it: killing the
        // timed-out CLI client alone leaves the container running (the runtime's
        // conmon/daemon owns it), so an infinite command would spin forever.
        let name = format!("cat-run-{}", uuid::Uuid::new_v4());
        let mut args = vec!["run".into(), "--rm".into(), "--name".into(), name.clone()];
        if cmd.stdin.is_some() {
            // Keep stdin open so the piped input reaches the command.
            args.push("-i".into());
        }
        self.shape(
            &mut args,
            cmd.cwd.as_deref(),
            &cmd.env,
            cmd.limits.cpu,
            cmd.limits.memory_mb,
            cmd.limits.network.as_deref(),
        );
        args.push(self.image.clone());
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
            Err(_) => {
                // The CLI client was killed on drop; remove the container itself
                // so the timed-out command stops running too (best-effort — `--rm`
                // cleans up the entry once it stops).
                let _ = self.capture(&["rm".into(), "-f".into(), name]).await;
                Ok(CommandResult {
                    exit_code: -1,
                    stdout: String::new(),
                    stderr: format!("command timed out after {}s", timeout.as_secs()),
                    timed_out: true,
                })
            }
        }
    }

    async fn open_session(&self, spec: SessionSpec) -> Result<Session> {
        let image = spec
            .image
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| self.image.clone());
        self.ensure_image(&image).await?;
        let shell = spec
            .shell
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "/bin/sh".to_string());

        // Start a keep-alive container with the workdir bind-mounted.
        let mut run_args = vec!["run".into(), "-d".into(), "--rm".into()];
        self.shape(
            &mut run_args,
            spec.cwd.as_deref(),
            &spec.env,
            spec.limits.cpu,
            spec.limits.memory_mb,
            spec.limits.network.as_deref(),
        );
        run_args.push(image);
        // Portable keep-alive (busybox `sleep` rejects `infinity`).
        run_args.push("tail".into());
        run_args.push("-f".into());
        run_args.push("/dev/null".into());
        let (code, out, err) = self.capture(&run_args).await?;
        if code != 0 {
            return Err(Error::provider(format!("failed to start container: {err}")));
        }
        let container_id = out.trim().to_string();
        if container_id.is_empty() {
            return Err(Error::provider("container start returned no id"));
        }

        // The PTY runs `<cli> exec -it <id> <shell>`; its host_dir is the bind
        // mount source (so an ephemeral session can be flushed to storage). The
        // env/shell were already applied to the container, so the PTY spec only
        // carries the host cwd + size. The shell may carry args (`bash
        // --noprofile`) — split it, or the runtime would exec a program literally
        // named "bash --noprofile".
        let (shell_prog, shell_args) = crate::pty::split_command(&shell);
        let mut exec_args = vec![
            "exec".into(),
            "-it".into(),
            container_id.clone(),
            shell_prog,
        ];
        exec_args.extend(shell_args);
        let pty_spec = SessionSpec {
            cwd: spec.cwd.clone(),
            cols: spec.cols,
            rows: spec.rows,
            ..Default::default()
        };
        let session = match self.sessions.open(self.cli(), &exec_args, &pty_spec, false) {
            Ok(s) => s,
            Err(e) => {
                let _ = self
                    .capture(&["rm".into(), "-f".into(), container_id])
                    .await;
                return Err(e);
            }
        };
        if let Ok(mut g) = self.containers.lock() {
            g.insert(session.id.clone(), container_id);
        }
        Ok(session)
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
        let container_id = self
            .containers
            .lock()
            .ok()
            .and_then(|mut g| g.remove(&session.id));
        if let Some(id) = container_id {
            let _ = self.capture(&["rm".into(), "-f".into(), id]).await;
        }
        Ok(())
    }

    async fn reap(&self) -> Result<Vec<String>> {
        // A self-exited PTY (`<cli> exec` finished) leaves the keep-alive
        // container running — remove it too, not just the PTY entry.
        let dead = self.sessions.reap_exited()?;
        for id in &dead {
            let container_id = self.containers.lock().ok().and_then(|mut g| g.remove(id));
            if let Some(cid) = container_id {
                let _ = self.capture(&["rm".into(), "-f".into(), cid]).await;
            }
        }
        Ok(dead)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn podman_available() -> bool {
        std::process::Command::new("podman")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Open a busybox container session, run a command, read its output, close.
    /// Skips when podman (or the image) is unavailable.
    #[tokio::test]
    async fn container_session_runs_a_command_and_cleans_up() {
        if !podman_available() {
            eprintln!("skipping container_session: podman not available");
            return;
        }
        let exec = ContainerExecutor::new("podman", "docker.io/library/busybox:latest", "none");
        let session = match exec
            .open_session(SessionSpec {
                shell: Some("/bin/sh".into()),
                ..Default::default()
            })
            .await
        {
            Ok(s) => s,
            Err(e) => {
                eprintln!("skipping container_session: start failed: {e}");
                return;
            }
        };

        exec.session_write(&session, b"echo MARK$((6*7))\n".to_vec())
            .await
            .expect("write");
        let mut got = String::new();
        for _ in 0..150 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            got.push_str(&String::from_utf8_lossy(
                &exec.session_read(&session, 0).await.expect("read"),
            ));
            if got.contains("MARK42") {
                break;
            }
        }
        let saw_output = got.contains("MARK42");
        exec.close_session(&session).await.expect("close");
        assert!(saw_output, "expected container command output, got {got:?}");
    }
}
