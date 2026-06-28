//! Shared PTY machinery for the interactive-session backends (Local / Sandbox,
//! SOUL §20). A terminal session is a long-lived shell on a pseudo-terminal: the
//! agent writes input (`session_write`), drains accumulated output
//! (`session_read`), and a read-only web pane tails a live byte stream
//! (`session_output`). The same [`SessionStore`] backs the container/k8s
//! backends in later slices via their own spawn paths.
//!
//! `portable_pty` is a blocking API, so each session runs a dedicated reader
//! thread that pumps PTY bytes into a bounded drain buffer (for the agent) and a
//! `tokio::sync::broadcast` (for live panes). Writes/kills hop to
//! `spawn_blocking` so the async runtime never blocks on PTY I/O.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use catalerum_core::error::{Error, Result};
use catalerum_core::provider::{ByteStream, Session, SessionSpec};
use futures::stream::{self, StreamExt};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use tokio::sync::broadcast;

/// Max bytes of unread output buffered per session before the oldest is dropped
/// (bounds memory when the agent never reads).
const DRAIN_CAP: usize = 256 * 1024;
/// Broadcast backlog for live-pane subscribers; older frames are dropped on lag.
const BROADCAST_CAP: usize = 1024;
/// Default terminal size when a [`SessionSpec`] leaves it unset.
const DEFAULT_COLS: u16 = 120;
const DEFAULT_ROWS: u16 = 30;

/// One live PTY session: the master (for resize), the writer, the child (killed
/// on close), an unread-output drain buffer, the live broadcast, and the host
/// working directory (used to flush an ephemeral session to storage).
struct LiveSession {
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    drain: Arc<Mutex<VecDeque<u8>>>,
    output: broadcast::Sender<Vec<u8>>,
    host_dir: Option<String>,
}

/// A shared registry of live PTY sessions, reusable by any PTY-backed executor.
/// Cloneable (an `Arc` inside) so the executor and its session-IO methods share
/// the same live set.
#[derive(Clone)]
pub struct SessionStore {
    sessions: Arc<Mutex<HashMap<String, Arc<LiveSession>>>>,
}

impl Default for SessionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for SessionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let live = self.sessions.lock().map(|m| m.len()).unwrap_or(0);
        f.debug_struct("SessionStore").field("live", &live).finish()
    }
}

impl SessionStore {
    #[must_use]
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn lookup(&self, id: &str) -> Result<Arc<LiveSession>> {
        self.sessions
            .lock()
            .map_err(|_| Error::provider("terminal session registry poisoned"))?
            .get(id)
            .cloned()
            .ok_or_else(|| Error::invalid(format!("unknown terminal session `{id}`")))
    }

    /// Open a PTY running `program args…` shaped by `spec`. When `scrub_env` is
    /// set (sandbox), the inherited environment is dropped and replaced with a
    /// minimal `PATH`/`TERM` (+ `HOME` = the cwd); otherwise the parent
    /// environment is inherited. `spec.env` is applied last either way.
    pub fn open(
        &self,
        program: &str,
        args: &[String],
        spec: &SessionSpec,
        scrub_env: bool,
    ) -> Result<Session> {
        let cols = if spec.cols == 0 {
            DEFAULT_COLS
        } else {
            spec.cols
        };
        let rows = if spec.rows == 0 {
            DEFAULT_ROWS
        } else {
            spec.rows
        };
        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };

        let pair = native_pty_system()
            .openpty(size)
            .map_err(|e| Error::provider(format!("openpty failed: {e}")))?;

        let mut cmd = CommandBuilder::new(program);
        cmd.args(args);
        if scrub_env {
            cmd.env_clear();
            cmd.env(
                "PATH",
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            );
            cmd.env("TERM", "xterm-256color");
        }
        if let Some(cwd) = &spec.cwd {
            cmd.cwd(cwd);
            if scrub_env {
                cmd.env("HOME", cwd);
            }
        }
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| Error::provider(format!("failed to spawn `{program}`: {e}")))?;
        // Drop the slave handle so the master reader sees EOF when the child exits.
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| Error::provider(format!("pty reader: {e}")))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| Error::provider(format!("pty writer: {e}")))?;

        let (output, _) = broadcast::channel::<Vec<u8>>(BROADCAST_CAP);
        let drain = Arc::new(Mutex::new(VecDeque::new()));

        // Reader thread: blocking PTY reads → drain buffer (capped) + live fan-out.
        let drain_for_thread = Arc::clone(&drain);
        let output_for_thread = output.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let chunk = buf[..n].to_vec();
                        if let Ok(mut d) = drain_for_thread.lock() {
                            d.extend(chunk.iter().copied());
                            let over = d.len().saturating_sub(DRAIN_CAP);
                            for _ in 0..over {
                                d.pop_front();
                            }
                        }
                        // A send with no live subscribers is fine (Err ignored).
                        let _ = output_for_thread.send(chunk);
                    }
                }
            }
        });

        let id = uuid::Uuid::new_v4().to_string();
        let live = Arc::new(LiveSession {
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            child: Mutex::new(child),
            drain,
            output,
            host_dir: spec.cwd.clone(),
        });
        self.sessions
            .lock()
            .map_err(|_| Error::provider("terminal session registry poisoned"))?
            .insert(id.clone(), live);

        Ok(Session {
            id,
            host_dir: spec.cwd.clone(),
            // The PTY layer only knows host paths; a backend whose files live
            // inside a container fills `cwd` itself after opening.
            cwd: None,
        })
    }

    /// Write bytes (keystrokes / a command line) to a session's PTY input.
    pub async fn write(&self, id: &str, data: Vec<u8>) -> Result<()> {
        let live = self.lookup(id)?;
        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut w = live
                .writer
                .lock()
                .map_err(|_| Error::provider("terminal writer poisoned"))?;
            w.write_all(&data)
                .and_then(|()| w.flush())
                .map_err(|e| Error::provider(format!("terminal write failed: {e}")))
        })
        .await
        .map_err(|e| Error::provider(format!("terminal write task failed: {e}")))?
    }

    /// Drain up to `max_bytes` (0 = all) of output buffered since the last read.
    pub fn read(&self, id: &str, max_bytes: usize) -> Result<Vec<u8>> {
        let live = self.lookup(id)?;
        let mut d = live
            .drain
            .lock()
            .map_err(|_| Error::provider("terminal drain poisoned"))?;
        let take = if max_bytes == 0 {
            d.len()
        } else {
            max_bytes.min(d.len())
        };
        Ok(d.drain(..take).collect())
    }

    /// Subscribe to a session's live output (bytes produced from now on), for a
    /// read-only pane. Independent of the agent's [`read`](Self::read) drain.
    pub fn output(&self, id: &str) -> Result<ByteStream> {
        let live = self.lookup(id)?;
        let rx = live.output.subscribe();
        let s = stream::unfold(rx, |mut rx| async move {
            loop {
                match rx.recv().await {
                    Ok(bytes) => return Some((Ok(bytes), rx)),
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        })
        .boxed();
        Ok(s)
    }

    /// Resize a session's PTY.
    pub fn resize(&self, id: &str, cols: u16, rows: u16) -> Result<()> {
        let live = self.lookup(id)?;
        let master = live
            .master
            .lock()
            .map_err(|_| Error::provider("terminal master poisoned"))?;
        master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| Error::provider(format!("terminal resize failed: {e}")))
    }

    /// Close a session: remove it and kill its child (the reader thread then
    /// sees EOF and exits, ending any live `output` streams). Idempotent.
    pub async fn close(&self, id: &str) -> Result<()> {
        let live = self
            .sessions
            .lock()
            .map_err(|_| Error::provider("terminal session registry poisoned"))?
            .remove(id);
        if let Some(live) = live {
            tokio::task::spawn_blocking(move || {
                if let Ok(mut c) = live.child.lock() {
                    let _ = c.kill();
                    let _ = c.wait();
                }
            })
            .await
            .map_err(|e| Error::provider(format!("terminal close task failed: {e}")))?;
        }
        Ok(())
    }

    /// The host working directory of a live session (for the ephemeral flush).
    pub fn host_dir(&self, id: &str) -> Result<Option<String>> {
        Ok(self.lookup(id)?.host_dir.clone())
    }

    /// Remove and return the ids of sessions whose child process has already
    /// exited on its own — the user ran `exit`, the shell crashed, the command
    /// finished — without an explicit [`close`](Self::close). Non-blocking
    /// (`try_wait`, never waits on a live child) and also reaps the OS zombie.
    /// A backend uses the returned ids to tear down any external resource
    /// (container / Pod) it kept for those sessions.
    pub fn reap_exited(&self) -> Result<Vec<String>> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| Error::provider("terminal session registry poisoned"))?;
        let mut dead = Vec::new();
        sessions.retain(|id, live| {
            let exited = live
                .child
                .lock()
                .ok()
                .and_then(|mut c| c.try_wait().ok().flatten())
                .is_some();
            if exited {
                dead.push(id.clone());
            }
            !exited
        });
        Ok(dead)
    }
}

/// Resolve the interactive shell to launch for a **local/sandbox host** PTY into
/// a `(program, args)` pair.
///
/// A non-empty `configured` shell (`[exec].shell`) wins and may itself carry
/// arguments (whitespace-separated), e.g. `bash --noprofile` or
/// `/usr/bin/env -S fish`. Otherwise we use a deterministic, **wizard-free**
/// default — never the user's `$SHELL`, which can be an interactive zsh/fish
/// whose first-run new-user setup (`zsh-newuser-install`, fish's config wizard)
/// prints a menu and then **blocks the PTY waiting for a keypress**, so the shell
/// never runs the agent's command and every `session_read` comes back empty. The
/// default is `/usr/bin/env bash` on Unix and PowerShell on Windows (`pwsh` when
/// it's on `PATH`, else `powershell.exe`); both start clean on a fresh PTY.
#[must_use]
pub fn resolve_shell(configured: Option<&str>) -> (String, Vec<String>) {
    match configured.map(str::trim).filter(|s| !s.is_empty()) {
        Some(cmd) => split_command(cmd),
        None => default_shell_command(),
    }
}

/// The platform default host shell as `(program, args)` — see [`resolve_shell`].
fn default_shell_command() -> (String, Vec<String>) {
    #[cfg(windows)]
    {
        // Prefer PowerShell Core (`pwsh`); fall back to Windows PowerShell. Pass the
        // resolved full path when found so the spawn doesn't depend on `CreateProcess`
        // PATH search.
        for exe in ["pwsh.exe", "powershell.exe"] {
            if let Some(p) = which_on_path(exe) {
                return (p.to_string_lossy().into_owned(), Vec::new());
            }
        }
        ("powershell.exe".to_string(), Vec::new())
    }
    #[cfg(not(windows))]
    {
        // `/usr/bin/env bash` resolves bash on `PATH`; fall back to `/bin/sh` only
        // when bash is genuinely unavailable (both are wizard-free on a fresh PTY).
        if which_on_path("bash").is_some() {
            ("/usr/bin/env".to_string(), vec!["bash".to_string()])
        } else {
            ("/bin/sh".to_string(), Vec::new())
        }
    }
}

/// Split a configured shell command into `(program, args)` on whitespace. The
/// first token is the program; the rest are arguments (so `[exec].shell` may
/// carry flags). An empty string yields `/bin/sh` (the caller filters empties).
/// Shared with the container backends, whose `exec` argv would otherwise treat
/// an arg-carrying shell (`bash --noprofile`) as one literal program name.
pub(crate) fn split_command(cmd: &str) -> (String, Vec<String>) {
    let mut it = cmd.split_whitespace().map(str::to_string);
    let program = it.next().unwrap_or_else(|| "/bin/sh".to_string());
    (program, it.collect())
}

/// First `PATH` directory containing an executable named `name`, if any. Used to
/// probe for `bash` / `pwsh` when picking the default host shell.
fn which_on_path(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `resolve_shell` honours a configured shell (splitting off its args) and
    /// otherwise picks a deterministic, wizard-free default — never `$SHELL`.
    #[test]
    fn resolve_shell_prefers_config_then_a_safe_default() {
        // A configured shell wins and its arguments are split off.
        assert_eq!(
            resolve_shell(Some("/bin/bash --noprofile -l")),
            (
                "/bin/bash".to_string(),
                vec!["--noprofile".into(), "-l".into()]
            )
        );
        assert_eq!(
            resolve_shell(Some("/usr/bin/env bash")),
            ("/usr/bin/env".to_string(), vec!["bash".to_string()])
        );
        // Empty / whitespace-only config falls through to the platform default.
        let (prog, _) = resolve_shell(Some("   "));
        let (dprog, _) = resolve_shell(None);
        assert_eq!(prog, dprog);
        // The default never resolves to the user's interactive `$SHELL`.
        if let Some(user_shell) = std::env::var_os("SHELL") {
            assert_ne!(
                std::path::Path::new(&dprog),
                std::path::Path::new(&user_shell),
                "default shell must not be the user's $SHELL (it may launch a wizard)"
            );
        }
        // On Unix the default is bash via env (or /bin/sh if bash is absent).
        #[cfg(not(windows))]
        assert!(
            dprog == "/usr/bin/env" || dprog == "/bin/sh",
            "unexpected unix default shell: {dprog}"
        );
    }

    /// `reap_exited` removes a session whose child has exited and leaves a live
    /// one running — the basis of the manager's self-exit reaper (SOUL §20).
    #[tokio::test]
    async fn reap_exited_collects_finished_sessions_and_keeps_live_ones() {
        let store = SessionStore::new();
        // A shell that exits straight away.
        let exited = store
            .open(
                "/bin/sh",
                &["-c".into(), "exit 0".into()],
                &SessionSpec::default(),
                false,
            )
            .expect("open exiting shell");
        // An interactive shell that stays alive (no `exit` written to it).
        let live = store
            .open("/bin/sh", &[], &SessionSpec::default(), false)
            .expect("open live shell");

        let mut reaped = Vec::new();
        for _ in 0..200 {
            reaped = store.reap_exited().expect("reap");
            if !reaped.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(reaped, vec![exited.id], "only the exited shell is reaped");
        // The live shell is untouched; a second pass finds nothing new.
        assert!(store.reap_exited().expect("reap2").is_empty());

        store.close(&live.id).await.expect("close live");
    }
}
