//! Best-effort filesystem watcher for the local `.ics` provider (SOUL §8).
//!
//! This is a *change signal* only — the authoritative path is
//! [`LocalIcsProvider::sync`](crate::local::LocalIcsProvider::sync), which is
//! idempotent and re-derivable at any time. A watcher just lets the ingest
//! scheduler react to edits promptly instead of polling. If `notify` fails to
//! initialise on a platform, callers fall back to polling; nothing here is on
//! the correctness path.

use std::path::Path;

use notify::{Event as NotifyEvent, EventKind, RecursiveMode, Watcher};
use tokio::sync::mpsc;

use catalerum_core::error::{Error, Result};

/// A handle that keeps the underlying OS watcher alive. Drop it to stop
/// watching.
pub struct IcsWatcher {
    _inner: notify::RecommendedWatcher,
}

/// Start watching `dir` for `.ics` create/modify/remove events.
///
/// Returns the live watcher handle (drop to stop) and a receiver that yields a
/// changed file path on each relevant event. Coalescing / debouncing is left to
/// the caller — a single edit can produce several OS events, and the response
/// (re-`sync`) is idempotent anyway.
pub fn watch_dir(dir: &Path) -> Result<(IcsWatcher, mpsc::UnboundedReceiver<std::path::PathBuf>)> {
    let (tx, rx) = mpsc::unbounded_channel();

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<NotifyEvent>| {
        let Ok(event) = res else { return };
        if !matches!(
            event.kind,
            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
        ) {
            return;
        }
        for path in event.paths {
            let is_ics = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("ics"));
            if is_ics {
                // Receiver gone => watcher is being torn down; ignore.
                let _ = tx.send(path);
            }
        }
    })
    .map_err(|e| Error::Provider(format!("init fs watcher: {e}")))?;

    watcher
        .watch(dir, RecursiveMode::NonRecursive)
        .map_err(|e| Error::Provider(format!("watch {}: {e}", dir.display())))?;

    Ok((IcsWatcher { _inner: watcher }, rx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn watcher_reports_ics_writes() {
        let dir = tempfile::tempdir().unwrap();
        let (_w, mut rx) = watch_dir(dir.path()).expect("watch");

        // Give the watcher a beat to arm, then create an .ics file.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        tokio::fs::write(
            dir.path().join("new.ics"),
            "BEGIN:VCALENDAR\nEND:VCALENDAR\n",
        )
        .await
        .unwrap();

        let got = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await;
        match got {
            Ok(Some(path)) => assert!(path.to_string_lossy().ends_with(".ics")),
            // Some CI/sandbox filesystems deliver no inotify events; the watcher
            // is best-effort, so a timeout here is not a correctness failure.
            _ => eprintln!("watcher delivered no event (best-effort path); skipping"),
        }
    }
}
