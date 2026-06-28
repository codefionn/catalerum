//! catalerum-exec — the [`Executor`](catalerum_core::provider::Executor) backends
//! (SOUL §20).
//!
//! The LLM and automations run shell commands / code via the `run_command` tool
//! (and skill code) — **never** directly. Execution goes through a pluggable
//! `Executor`, gated by `exec:*` capabilities (§19) and an allow-list.
//!
//! # What's here
//! - [`LocalExecutor`] — `tokio::process` on the host. Highest blast radius;
//!   **protected and opt-in** (§20). Runs `argv` (not inline `code` — that is the
//!   container/bao backends' job), with an optional program **allow-list**, a
//!   wall-clock timeout (the child is killed on elapse), and stdin/env/cwd.
//!
//! Container (docker/podman), Kubernetes, and bao backends land in later slices.
//! The default posture stays deny-by-default: a command runs only when an
//! executor is configured, the caller holds `exec:run`, and (for [`LocalExecutor`])
//! the program is allow-listed.

#![forbid(unsafe_code)]

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;

use catalerum_core::error::{Error, Result};
use catalerum_core::provider::{
    ByteStream, CommandResult, CommandSpec, Executor, Session, SessionSpec,
};

mod container;
mod k8s;
mod proc;
mod pty;
mod sandbox;

pub use container::ContainerExecutor;
pub use k8s::KubernetesExecutor;
use proc::{finish_capture, read_capped};
pub use pty::{resolve_shell, SessionStore};
pub use sandbox::{
    sandbox_name, volume_name, K8sSandbox, PodmanSandbox, SandboxHandle, SandboxPhase, SandboxSpec,
    SandboxStatus, WorkspaceSandbox,
};

/// Default wall-clock timeout for a command that doesn't set one.
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Default per-stream cap on captured stdout/stderr. A runaway command (`yes`,
/// `cat` of a huge file, …) can emit gigabytes in well under the wall-clock
/// timeout; `wait_with_output` would buffer all of it and OOM the worker. We keep
/// at most this many bytes per stream and **drain the rest** (so the child never
/// blocks on a full pipe and still runs to completion / its timeout).
const DEFAULT_MAX_CAPTURE_BYTES: usize = 1 << 20; // 1 MiB

/// Runs commands as host processes via `tokio::process` (SOUL §20). The highest
/// blast radius backend — **protected, opt-in only**. An optional `allow` list of
/// permitted program names (matched on the basename or the exact `argv[0]`) is
/// the executor-level deny-by-default gate, complementing the `exec:run`
/// capability check at the tool layer.
#[derive(Clone, Debug)]
pub struct LocalExecutor {
    /// Allowed program names; `None` permits any program (still capability-gated
    /// upstream). `Some(list)` rejects anything not listed.
    allow: Option<Vec<String>>,
    /// Per-stream cap on captured stdout/stderr bytes (see
    /// [`DEFAULT_MAX_CAPTURE_BYTES`]).
    max_capture_bytes: usize,
    /// Drop the inherited environment (replacing it with a minimal `PATH`) for
    /// both one-shot `run` and interactive sessions — the **sandbox** posture
    /// (SOUL §20). Set via [`LocalExecutor::sandboxed`].
    scrub_env: bool,
    /// Live interactive PTY sessions (SOUL §20).
    sessions: SessionStore,
}

impl Default for LocalExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalExecutor {
    /// An executor that permits any program (still `exec:run`-gated upstream).
    #[must_use]
    pub fn new() -> Self {
        Self {
            allow: None,
            max_capture_bytes: DEFAULT_MAX_CAPTURE_BYTES,
            scrub_env: false,
            sessions: SessionStore::new(),
        }
    }

    /// An executor restricted to the given program names (basename or exact
    /// `argv[0]`).
    #[must_use]
    pub fn with_allow_list(allow: Vec<String>) -> Self {
        Self {
            allow: Some(allow),
            ..Self::new()
        }
    }

    /// Override the per-stream stdout/stderr capture cap (builder).
    #[must_use]
    pub fn with_max_capture_bytes(mut self, max: usize) -> Self {
        self.max_capture_bytes = max;
        self
    }

    /// The **sandbox** variant (SOUL §20): the inherited environment is scrubbed
    /// (replaced with a minimal `PATH`) for `run` and for interactive sessions —
    /// lightweight isolation without a container. Combine with a confined `cwd`.
    #[must_use]
    pub fn sandboxed(mut self) -> Self {
        self.scrub_env = true;
        self
    }

    /// Whether `program` is permitted by the allow-list (always true when no
    /// list is configured).
    fn permits(&self, program: &str) -> bool {
        match &self.allow {
            None => true,
            Some(list) => {
                let base = Path::new(program)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or(program);
                list.iter().any(|a| a == program || a == base)
            }
        }
    }
}

#[async_trait]
impl Executor for LocalExecutor {
    async fn run(&self, cmd: CommandSpec) -> Result<CommandResult> {
        if cmd.code.is_some() {
            return Err(Error::Unsupported(
                "local executor runs argv, not inline code (use the container/bao backend)".into(),
            ));
        }
        let program = cmd
            .argv
            .first()
            .ok_or_else(|| Error::invalid("run_command requires a non-empty argv"))?;
        if !self.permits(program) {
            return Err(Error::unauthorized(format!(
                "program `{program}` is not on the executor allow-list"
            )));
        }

        let mut command = tokio::process::Command::new(program);
        command
            .args(&cmd.argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Kill the child if the timeout drops the wait future (no leak).
            .kill_on_drop(true);
        // Sandbox posture: drop the inherited environment (a minimal PATH
        // remains) before applying the caller's explicit env.
        if self.scrub_env {
            command.env_clear();
            command.env(
                "PATH",
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            );
            // A scrubbed env with no HOME breaks many tools (git/python/npm read
            // `~`); default it to the confined cwd, matching the interactive PTY
            // path. Set before the caller's env so an explicit HOME still wins.
            if let Some(cwd) = &cmd.cwd {
                command.env("HOME", cwd);
            }
        }
        for (k, v) in &cmd.env {
            command.env(k, v);
        }
        if let Some(cwd) = &cmd.cwd {
            command.current_dir(cwd);
        }

        let mut child = command
            .spawn()
            .map_err(|e| Error::provider(format!("failed to spawn `{program}`: {e}")))?;

        // Take all three pipes up front so the concurrent block borrows only
        // `child` (for `wait`). Feeding stdin, draining stdout, and draining stderr
        // must run **concurrently**: writing the whole stdin first and only then
        // reading deadlocks a command that echoes large input (`cat`), since both
        // OS pipe buffers fill (~64 KiB each) — the child blocks on stdout, stops
        // reading stdin, and our write blocks forever. It also kept the stdin write
        // *outside* the timeout below, so such a hang was unkillable.
        let stdin_pipe = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let input = cmd.stdin;

        // Capture stdout/stderr concurrently with the wait and the stdin feed, each
        // stream capped (the rest drained) so a runaway command can't OOM the worker
        // and no pipe filling can deadlock the child. `wait` only *borrows* the
        // child, so on a timeout the child is still dropped at scope end →
        // `kill_on_drop` reaps it (and the half-written stdin pipe is dropped too).
        let cap = self.max_capture_bytes;
        let timeout = Duration::from_secs(cmd.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
        let collect = async move {
            // Feed stdin (if any), then close it (drop on scope exit) so the child
            // sees EOF and isn't left waiting on input.
            let feed = async move {
                if let Some(mut stdin) = stdin_pipe {
                    if let Some(input) = input {
                        let _ = stdin.write_all(input.as_bytes()).await;
                    }
                    let _ = stdin.shutdown().await;
                }
            };
            tokio::join!(
                child.wait(),
                feed,
                read_capped(stdout, cap),
                read_capped(stderr, cap),
            )
        };
        match tokio::time::timeout(timeout, collect).await {
            Ok((status, (), (out, out_cut), (err, err_cut))) => {
                let status =
                    status.map_err(|e| Error::provider(format!("command i/o failed: {e}")))?;
                Ok(CommandResult {
                    exit_code: status.code().unwrap_or(-1),
                    stdout: finish_capture(out, out_cut, cap),
                    stderr: finish_capture(err, err_cut, cap),
                    timed_out: false,
                })
            }
            // Timeout: the join future is dropped → `kill_on_drop` reaps the child.
            Err(_) => Ok(CommandResult {
                exit_code: -1,
                stdout: String::new(),
                stderr: format!("command timed out after {}s", timeout.as_secs()),
                timed_out: true,
            }),
        }
    }

    async fn open_session(&self, spec: SessionSpec) -> Result<Session> {
        // The session's program is an interactive shell: a pinned `spec.shell`
        // (which may carry args), else a deterministic, wizard-free platform
        // default — never the user's interactive `$SHELL`, whose first-run setup
        // can block the PTY (see `resolve_shell`). An allow-list (when set) must
        // still permit the program — otherwise sessions would bypass the gate.
        let (program, args) = resolve_shell(spec.shell.as_deref());
        if !self.permits(&program) {
            return Err(Error::unauthorized(format!(
                "shell `{program}` is not on the executor allow-list"
            )));
        }
        self.sessions.open(&program, &args, &spec, self.scrub_env)
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
        self.sessions.close(&session.id).await
    }

    async fn reap(&self) -> Result<Vec<String>> {
        // No external resource beyond the PTY child — `reap_exited` clears it.
        self.sessions.reap_exited()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> CommandSpec {
        CommandSpec {
            argv: parts.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn runs_a_command_and_captures_stdout_and_exit() {
        let exec = LocalExecutor::new();
        let out = exec.run(argv(&["echo", "hello world"])).await.unwrap();
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.stdout.trim(), "hello world");
        assert!(!out.timed_out);

        // Non-zero exit is reported, not an error.
        let out = exec.run(argv(&["false"])).await.unwrap();
        assert_ne!(out.exit_code, 0);
    }

    #[tokio::test]
    async fn pipes_stdin_and_sets_env() {
        let exec = LocalExecutor::new();
        let cat = CommandSpec {
            argv: vec!["cat".into()],
            stdin: Some("piped input".into()),
            ..Default::default()
        };
        assert_eq!(exec.run(cat).await.unwrap().stdout, "piped input");

        let env_cmd = CommandSpec {
            argv: vec!["sh".into(), "-c".into(), "printf %s \"$FOO\"".into()],
            env: vec![("FOO".into(), "bar".into())],
            ..Default::default()
        };
        assert_eq!(exec.run(env_cmd).await.unwrap().stdout, "bar");
    }

    #[tokio::test]
    async fn sandboxed_run_defaults_home_to_cwd() {
        let exec = LocalExecutor::new().sandboxed();
        let dir = std::env::temp_dir().to_string_lossy().into_owned();
        // A scrubbed command still has HOME (else git/python/npm break); it defaults
        // to the confined cwd.
        let cmd = CommandSpec {
            argv: vec!["/bin/sh".into(), "-c".into(), "printf %s \"$HOME\"".into()],
            cwd: Some(dir.clone()),
            ..Default::default()
        };
        assert_eq!(exec.run(cmd).await.unwrap().stdout, dir);

        // An explicit HOME from the caller still wins over the cwd default.
        let overridden = CommandSpec {
            argv: vec!["/bin/sh".into(), "-c".into(), "printf %s \"$HOME\"".into()],
            cwd: Some(dir.clone()),
            env: vec![("HOME".into(), "/custom/home".into())],
            ..Default::default()
        };
        assert_eq!(exec.run(overridden).await.unwrap().stdout, "/custom/home");
    }

    #[tokio::test]
    async fn allow_list_rejects_disallowed_programs() {
        let exec = LocalExecutor::with_allow_list(vec!["echo".into()]);
        assert!(exec.run(argv(&["echo", "ok"])).await.is_ok());
        // Basename match works even with a path.
        assert!(exec.run(argv(&["/bin/echo", "ok"])).await.is_ok());
        // `cat` is not listed → rejected before spawn.
        let err = exec.run(argv(&["cat"])).await.unwrap_err();
        assert!(matches!(err, Error::Unauthorized(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn times_out_and_kills_the_child() {
        let exec = LocalExecutor::new();
        let slow = CommandSpec {
            argv: vec!["sleep".into(), "10".into()],
            timeout_secs: Some(1),
            ..Default::default()
        };
        let out = exec.run(slow).await.unwrap();
        assert!(out.timed_out, "should report a timeout");
    }

    #[tokio::test]
    async fn large_stdin_and_stdout_do_not_deadlock() {
        // `cat` echoes a 256 KiB stdin back to stdout. If the whole stdin is
        // written *before* stdout is drained, both pipe buffers fill (~64 KiB each)
        // and the child deadlocks — and since the stdin write used to sit outside
        // the timeout, run() would hang forever. Feeding stdin concurrently with the
        // capped reads fixes it; the 1 MiB default cap (> 256 KiB) keeps it verbatim.
        let exec = LocalExecutor::new();
        let input = "x".repeat(256 * 1024);
        let cmd = CommandSpec {
            argv: vec!["cat".into()],
            stdin: Some(input.clone()),
            timeout_secs: Some(10),
            ..Default::default()
        };
        let out = exec.run(cmd).await.unwrap();
        assert!(!out.timed_out, "must not deadlock or time out");
        assert_eq!(out.exit_code, 0);
        assert_eq!(
            out.stdout.len(),
            input.len(),
            "full echo captured, no marker"
        );
    }

    #[tokio::test]
    async fn large_output_is_capped_and_marked() {
        // 1000 bytes of stdout against a 16-byte cap: the capture stays bounded
        // (no OOM on a runaway command), keeps the head, and flags truncation.
        let exec = LocalExecutor::new().with_max_capture_bytes(16);
        let cmd = CommandSpec {
            argv: vec![
                "sh".into(),
                "-c".into(),
                "head -c 1000 /dev/zero | tr '\\0' 'a'".into(),
            ],
            ..Default::default()
        };
        let out = exec.run(cmd).await.unwrap();
        assert_eq!(out.exit_code, 0);
        assert!(!out.timed_out);
        let (data, marker) = out.stdout.split_once('\n').unwrap_or((&out.stdout, ""));
        assert_eq!(data, "a".repeat(16), "captured data is bounded to the cap");
        assert!(
            marker.contains("truncated"),
            "truncation is marked: {marker:?}"
        );
    }

    #[tokio::test]
    async fn small_output_is_not_marked() {
        let exec = LocalExecutor::new(); // 1 MiB default cap
        let out = exec.run(argv(&["echo", "hi"])).await.unwrap();
        assert_eq!(out.stdout, "hi\n", "short output is verbatim, no marker");
    }

    #[tokio::test]
    async fn inline_code_is_unsupported_and_empty_argv_is_invalid() {
        let exec = LocalExecutor::new();
        let code = CommandSpec {
            code: Some("print('hi')".into()),
            language: Some("python".into()),
            ..Default::default()
        };
        assert!(matches!(exec.run(code).await, Err(Error::Unsupported(_))));
        assert!(matches!(
            exec.run(CommandSpec::default()).await,
            Err(Error::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn interactive_session_runs_a_command_and_closes() {
        // Open a PTY shell, type a command, and drain its echoed output.
        let exec = LocalExecutor::new();
        let session = exec
            .open_session(SessionSpec::default())
            .await
            .expect("open session");
        assert!(!session.id.is_empty());

        exec.session_write(&session, b"echo done_marker_42\n".to_vec())
            .await
            .expect("write");

        let mut got = String::new();
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let bytes = exec.session_read(&session, 0).await.expect("read");
            got.push_str(&String::from_utf8_lossy(&bytes));
            if got.contains("done_marker_42") {
                break;
            }
        }
        assert!(
            got.contains("done_marker_42"),
            "expected the command output on the PTY, got {got:?}"
        );

        exec.close_session(&session).await.expect("close");
        // The session is gone: reads/writes now fail (not panic).
        assert!(
            exec.session_read(&session, 0).await.is_err(),
            "read on a closed session must error"
        );
    }

    #[tokio::test]
    async fn open_session_respects_the_allow_list() {
        // An allow-list that excludes the shell rejects opening a session.
        let exec = LocalExecutor::with_allow_list(vec!["definitely-not-a-shell".into()]);
        let err = exec
            .open_session(SessionSpec::default())
            .await
            .expect_err("shell not allow-listed");
        assert!(matches!(err, Error::Unauthorized(_)), "got {err:?}");
    }
}
