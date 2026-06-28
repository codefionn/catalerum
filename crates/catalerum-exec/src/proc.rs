//! Shared child-process plumbing for the CLI-driven backends (SOUL §20).
//!
//! Every backend that shells out to a CLI (`podman`/`docker`/`kubectl`) used to
//! collect its output with `Command::output()`, which has two latent failure
//! modes the [`LocalExecutor`](crate::LocalExecutor) was already hardened
//! against:
//!
//! - **Unbounded capture** — `output()` buffers the whole stdout/stderr, so a
//!   runaway command in a container (`yes`, `cat` of a huge file) OOMs the api
//!   worker well before its wall-clock timeout.
//! - **No kill-on-drop** — dropping the `output()` future (how
//!   `tokio::time::timeout` cancels) leaves the CLI child running, so every
//!   timed-out `run` leaked a live `podman`/`kubectl` process (and kept the
//!   remote command running with it).
//!
//! [`capture_capped`] is the one shared collector: piped stdin (fed
//! concurrently, so a large input can't deadlock on full pipes), per-stream
//! capped reads with the remainder drained, and `kill_on_drop` so a timeout
//! reaps the client. Backends still owe their *remote* cleanup on timeout
//! (remove the named container / delete the Pod) — killing the CLI client alone
//! does not stop a container runtime's detached child.

use std::process::Stdio;

use catalerum_core::error::{Error, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

/// Per-stream cap on captured CLI stdout/stderr — the same bound the local
/// executor defaults to (its comment explains the OOM rationale).
pub(crate) const MAX_CLI_CAPTURE_BYTES: usize = 1 << 20; // 1 MiB

/// Run `program args…` to completion: optional `stdin` (piped + closed on EOF;
/// `None` gets a closed stdin so a child that reads input can't hang), capped
/// stdout/stderr capture, and `kill_on_drop` so cancelling the future (e.g. a
/// `tokio::time::timeout`) reaps the child instead of leaking it.
pub(crate) async fn capture_capped(
    program: &str,
    args: &[String],
    stdin: Option<String>,
) -> Result<(i32, String, String)> {
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|e| Error::provider(format!("`{program}` failed to start: {e}")))?;

    // Feed stdin concurrently with the capped reads (a sequential write of a
    // large input deadlocks once both OS pipe buffers fill — see LocalExecutor).
    let stdin_pipe = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let feed = async move {
        if let Some(mut pipe) = stdin_pipe {
            if let Some(input) = stdin {
                let _ = pipe.write_all(input.as_bytes()).await;
            }
            let _ = pipe.shutdown().await;
        }
    };
    let (status, (), (out, out_cut), (err, err_cut)) = tokio::join!(
        child.wait(),
        feed,
        read_capped(stdout, MAX_CLI_CAPTURE_BYTES),
        read_capped(stderr, MAX_CLI_CAPTURE_BYTES),
    );
    let status = status.map_err(|e| Error::provider(format!("`{program}` i/o failed: {e}")))?;
    Ok((
        status.code().unwrap_or(-1),
        finish_capture(out, out_cut, MAX_CLI_CAPTURE_BYTES),
        finish_capture(err, err_cut, MAX_CLI_CAPTURE_BYTES),
    ))
}

/// Read a child pipe to EOF, keeping at most `cap` bytes and **draining**
/// (discarding) the rest, so memory stays bounded and the child never blocks on a
/// full pipe. Returns the captured bytes and whether the stream exceeded `cap`.
pub(crate) async fn read_capped(
    pipe: Option<impl AsyncRead + Unpin>,
    cap: usize,
) -> (Vec<u8>, bool) {
    let Some(mut pipe) = pipe else {
        return (Vec::new(), false);
    };
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut truncated = false;
    loop {
        match pipe.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if buf.len() < cap {
                    let take = (cap - buf.len()).min(n);
                    buf.extend_from_slice(&chunk[..take]);
                    truncated |= take < n;
                } else {
                    truncated = true;
                }
            }
        }
    }
    (buf, truncated)
}

/// Lossily decode captured output, appending a marker when it was capped so a
/// downstream reader isn't misled into treating a partial capture as complete.
pub(crate) fn finish_capture(bytes: Vec<u8>, truncated: bool, cap: usize) -> String {
    let mut s = String::from_utf8_lossy(&bytes).into_owned();
    if truncated {
        s.push_str(&format!("\n[output truncated at {cap} bytes]"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn captures_exit_code_and_both_streams() {
        let (code, out, err) = capture_capped(
            "sh",
            &["-c".into(), "echo one; echo two >&2; exit 3".into()],
            None,
        )
        .await
        .unwrap();
        assert_eq!(code, 3);
        assert_eq!(out.trim(), "one");
        assert_eq!(err.trim(), "two");
    }

    #[tokio::test]
    async fn pipes_stdin_and_survives_large_echo() {
        // 256 KiB through `cat`: with a sequential stdin write this deadlocks
        // (both ~64 KiB pipe buffers fill); the concurrent feed keeps it moving.
        let input = "x".repeat(256 * 1024);
        let (code, out, _err) = capture_capped("cat", &[], Some(input.clone()))
            .await
            .unwrap();
        assert_eq!(code, 0);
        assert_eq!(out.len(), input.len());
    }

    #[tokio::test]
    async fn absent_stdin_is_closed_not_inherited() {
        // A child that reads stdin sees immediate EOF instead of hanging.
        let (code, out, _err) = capture_capped("sh", &["-c".into(), "cat; echo done".into()], None)
            .await
            .unwrap();
        assert_eq!(code, 0);
        assert_eq!(out.trim(), "done");
    }

    #[tokio::test]
    async fn runaway_output_is_capped_and_marked() {
        // 4 MiB of output against the 1 MiB cap: bounded capture + marker, and
        // the child still runs to completion (the remainder is drained).
        let (code, out, _err) = capture_capped(
            "sh",
            &[
                "-c".into(),
                "head -c 4194304 /dev/zero | tr '\\0' 'a'".into(),
            ],
            None,
        )
        .await
        .unwrap();
        assert_eq!(code, 0);
        assert!(out.len() <= MAX_CLI_CAPTURE_BYTES + 64);
        assert!(out.contains("truncated"), "cap marker present");
    }

    #[tokio::test]
    async fn timeout_kills_the_client() {
        // Dropping the future must reap the child (kill_on_drop) — the old
        // `output()` path left it running.
        let res = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            capture_capped("sleep", &["10".into()], None),
        )
        .await;
        assert!(res.is_err(), "the wait times out");
    }
}
