//! The scheduled backup worker (SOUL §30/§11): a tokio loop that runs
//! [`BackupEngine::run`] + [`BackupEngine::prune`] every `interval`,
//! single-firing each window across pods via the bus lock (SOUL §6.2).

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use catalerum_bus::Bus;

use crate::BackupEngine;

/// A background worker that takes a backup on a fixed interval.
///
/// Like the §11 schedulers, it sleeps then fires, with **no catch-up** across
/// restarts: a window missed during downtime is skipped, never replayed. Each
/// window is claimed via the bus lock keyed by the window's start instant, so
/// across multiple pods exactly one runs a given window (a no-op for the
/// in-process single-pod lock). A backup that overruns the interval simply
/// delays the next tick; it never overlaps itself within a pod (the loop is
/// sequential).
pub struct BackupWorker {
    engine: Arc<BackupEngine>,
    bus: Bus,
    interval: Duration,
}

impl BackupWorker {
    /// A worker running `engine` every `interval` (floored to 60s — a sub-minute
    /// backup cadence is a misconfiguration), single-firing via `bus`'s lock.
    #[must_use]
    pub fn new(engine: Arc<BackupEngine>, bus: Bus, interval: Duration) -> Self {
        let interval = interval.max(Duration::from_secs(60));
        Self {
            engine,
            bus,
            interval,
        }
    }

    /// Spawn the [`run`](Self::run) loop as a detached background task.
    #[must_use]
    pub fn spawn(self) -> JoinHandle<()> {
        tokio::spawn(self.run())
    }

    /// Tick forever: sleep one interval, claim the window, back up + prune. Any
    /// error is logged and retried next interval; the loop never exits.
    pub async fn run(self) {
        let secs = self.interval.as_secs().max(1) as i64;
        info!(
            interval_secs = secs,
            prefix = %self.engine.prefix(),
            "backup worker started"
        );
        loop {
            tokio::time::sleep(self.interval).await;

            // The window's start instant — the same on every pod for a given
            // wall-clock tick, so the lock key collides → single-fire. Held for a
            // whole interval so a slow loser never re-fires the same window.
            let window = Utc::now().timestamp().div_euclid(secs) * secs;
            let key = format!("backup-fire:{window}");
            match self.bus.lock().try_acquire(&key, self.interval).await {
                Ok(Some(_guard)) => {} // claimed — fall through and back up
                Ok(None) => continue,  // another pod is backing up this window
                Err(e) => {
                    warn!(error = %e, "backup fire-lock error; skipping this window");
                    continue;
                }
            }

            match self.engine.run().await {
                Ok(summary) => {
                    info!(
                        id = %summary.id,
                        tables = summary.tables,
                        rows = summary.rows,
                        objects = summary.objects,
                        "scheduled backup complete"
                    );
                    match self.engine.prune().await {
                        Ok(n) if n > 0 => info!(pruned = n, "pruned old backups"),
                        Ok(_) => {}
                        Err(e) => {
                            warn!(error = %e, "backup prune failed; will retry next interval")
                        }
                    }
                }
                Err(e) => warn!(error = %e, "scheduled backup failed; will retry next interval"),
            }
        }
    }
}
