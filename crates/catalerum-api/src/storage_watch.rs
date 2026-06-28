//! Storage watch worker (SOUL §9/§10) — keep a `watch`-enabled store's §10 index in
//! sync with its backend filesystem.
//!
//! For every [`watch`](crate::config::StorageBackendConfig::watch)-enabled store
//! (config or runtime, across all workspaces) this worker runs
//! [`scan_store`](crate::routes::storage::scan_store) to reconcile the catalogue
//! with the backend: new/changed files are catalogued + ingested, vanished ones are
//! purged. **Local** stores additionally get a real-time inotify watcher
//! (mirroring `catalerum-calendar`'s `watch_dir`) that triggers a prompt,
//! debounced re-scan on any create/modify/remove — so an edit to a watched
//! `~/Documents` is re-indexed within ~1s. **S3/WebDAV** have no native change
//! events, so the periodic pass (`[storage].watch_interval_secs`, default 60s) is
//! their sync path; for local stores it is a safety net on top of inotify.
//!
//! Scanning is idempotent, so overlapping passes/events are harmless. Errors are
//! logged per (workspace, store) and never crash the worker. The pass is
//! O(workspaces × watch-stores) backend listings per interval — bounded for the
//! single-tenant deployments browse/watch target; a busy multi-tenant deployment
//! should keep the interval generous.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use notify::{Event as NotifyEvent, EventKind, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use catalerum_core::model::ConnectionKind;
use catalerum_core::WorkspaceId;

use crate::routes::storage::{resolve, runtime_watch, scan_store, ScanEvents};
use crate::state::AppState;

/// Coalesce a burst of filesystem events into a single re-scan (an editor save can
/// emit several events; the scan is idempotent regardless).
const DEBOUNCE: Duration = Duration::from_millis(750);

/// A (workspace, store-name) the worker scans / watches.
type StoreKey = (WorkspaceId, String);

/// One watch-enabled store to reconcile: which workspace + store, and (for a local
/// backend) the directory to watch in real time.
struct WatchTarget {
    workspace: WorkspaceId,
    store: String,
    /// The local directory to inotify-watch, or `None` for a remote backend (poll
    /// only) / a local store with no path.
    local_path: Option<PathBuf>,
}

/// The storage watch worker (SOUL §9/§10). Build with [`new`](Self::new) and
/// [`spawn`](Self::spawn); it runs until the process exits.
pub struct StorageWatchWorker {
    state: AppState,
    interval: Duration,
}

impl StorageWatchWorker {
    /// Build the worker from app state, taking its re-scan cadence from
    /// `[storage].watch_interval_secs`.
    #[must_use]
    pub fn new(state: AppState) -> Self {
        let secs = state.config().storage.watch_interval_secs();
        Self {
            state,
            interval: Duration::from_secs(secs),
        }
    }

    /// Spawn the worker on the tokio runtime.
    pub fn spawn(self) -> JoinHandle<()> {
        tokio::spawn(self.run())
    }

    async fn run(self) {
        // Real-time change signals from the per-store inotify watchers funnel here.
        let (tx, mut rx) = mpsc::unbounded_channel::<StoreKey>();
        // Live OS watchers kept alive here (dropping one stops watching its dir).
        let mut watchers: HashMap<StoreKey, notify::RecommendedWatcher> = HashMap::new();
        let mut ticker = tokio::time::interval(self.interval);
        // `interval`'s first `tick()` returns immediately → an initial reconcile +
        // watcher-arming pass at startup (bootstraps pre-existing on-disk files).
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    self.reconcile_pass(&tx, &mut watchers).await;
                }
                Some(first) = rx.recv() => {
                    // Debounce: coalesce this signal with any others in the window.
                    let mut dirty: HashSet<StoreKey> = HashSet::new();
                    dirty.insert(first);
                    while let Ok(k) = rx.try_recv() {
                        dirty.insert(k);
                    }
                    tokio::time::sleep(DEBOUNCE).await;
                    while let Ok(k) = rx.try_recv() {
                        dirty.insert(k);
                    }
                    for (ws, store) in dirty {
                        self.scan_one(ws, &store).await;
                    }
                }
            }
        }
    }

    /// One periodic pass: enumerate every watch-enabled (workspace, store), scan
    /// each, and (re)arm a real-time inotify watcher for the local ones — dropping
    /// watchers whose target has disappeared (store deleted / watch turned off).
    async fn reconcile_pass(
        &self,
        tx: &mpsc::UnboundedSender<StoreKey>,
        watchers: &mut HashMap<StoreKey, notify::RecommendedWatcher>,
    ) {
        let targets = self.watch_targets().await;
        let desired: HashSet<StoreKey> = targets
            .iter()
            .map(|t| (t.workspace, t.store.clone()))
            .collect();
        watchers.retain(|k, _| desired.contains(k));

        for t in targets {
            let key = (t.workspace, t.store.clone());
            // Periodic reconcile — the sync path for remote stores, a safety net for
            // local ones (inotify drives their low-latency updates).
            self.scan_one(t.workspace, &t.store).await;
            // Arm a real-time watcher on a local store's directory (once).
            if let Some(path) = t.local_path.filter(|p| !p.as_os_str().is_empty()) {
                if let std::collections::hash_map::Entry::Vacant(slot) = watchers.entry(key) {
                    match start_watcher(&path, t.workspace, t.store.clone(), tx.clone()) {
                        Ok(w) => {
                            info!(store = %t.store, dir = %path.display(), "storage watch: watching directory");
                            slot.insert(w);
                        }
                        Err(e) => {
                            warn!(error = %e, store = %t.store, dir = %path.display(),
                                "storage watch: could not start fs watcher (will retry; periodic scan still runs)");
                        }
                    }
                }
            }
        }
    }

    /// Resolve + scan one store, logging the outcome. Best-effort: a failure is
    /// logged, never propagated (one bad store must not starve the others).
    async fn scan_one(&self, ws: WorkspaceId, store: &str) {
        let handle = match resolve(&self.state, ws, None, Some(store)).await {
            Ok(h) => h,
            Err(e) => {
                warn!(error = %e, %store, "storage watch: could not resolve store");
                return;
            }
        };
        match scan_store(self.state.store(), ws, &handle, "", ScanEvents::Fire).await {
            Ok(report) if report.indexed > 0 || report.removed > 0 => {
                info!(%store, indexed = report.indexed, removed = report.removed,
                    "storage watch: reconciled store");
            }
            Ok(_) => {}
            Err(e) => warn!(error = %e, %store, "storage watch: scan failed"),
        }
    }

    /// Every watch-enabled (workspace, store) across config + runtime backends.
    /// Config stores contribute one target per workspace they are
    /// [assigned to](crate::config::StorageBackendConfig::workspaces) (all of
    /// them when unassigned — their catalogue is per-workspace either way);
    /// runtime stores are per-workspace already.
    async fn watch_targets(&self) -> Vec<WatchTarget> {
        let store = self.state.store();
        let registry = self.state.storage();
        let workspaces = match store.workspaces().list().await {
            Ok(w) => w,
            Err(e) => {
                warn!(error = %e, "storage watch: listing workspaces");
                return Vec::new();
            }
        };
        // Config watch stores: (name, is-local, local_path, workspace assignment).
        let config_stores: Vec<(String, bool, String, Vec<String>)> = self
            .state
            .config()
            .storage
            .resolved_backends()
            .into_iter()
            .filter(|(_, c)| c.watch)
            .map(|(n, c)| {
                let is_local = c.kind() == Some("local");
                (n, is_local, c.local_path.clone(), c.workspaces)
            })
            .collect();
        // Connection names already represented by a config store (so a runtime
        // connection that one shadows isn't double-counted).
        let config_conn_names: HashSet<String> = registry
            .infos()
            .into_iter()
            .filter_map(|(n, _)| registry.get(&n).map(|s| s.connection.clone()))
            .collect();

        let mut out = Vec::new();
        for ws in &workspaces {
            for (name, is_local, local_path, assignment) in &config_stores {
                // A store assigned to other workspaces isn't scanned (or
                // inotify-watched) for this one — it can't resolve here anyway.
                if !crate::config::workspace_assigned(assignment, ws) {
                    continue;
                }
                out.push(WatchTarget {
                    workspace: ws.id,
                    store: name.clone(),
                    local_path: is_local.then(|| PathBuf::from(local_path)),
                });
            }
            let conns = match store.connections().list_by_workspace(ws.id).await {
                Ok(c) => c,
                Err(e) => {
                    warn!(error = %e, workspace = %ws.id, "storage watch: listing connections");
                    continue;
                }
            };
            for c in conns {
                if c.kind != ConnectionKind::Storage || config_conn_names.contains(&c.name) {
                    continue;
                }
                let Ok(row) = store.connections().get_row(ws.id, c.id).await else {
                    continue;
                };
                if !runtime_watch(row.config()) {
                    continue;
                }
                let is_local = row.config().get("kind").and_then(|v| v.as_str()) == Some("local");
                let local_path = is_local
                    .then(|| {
                        row.config()
                            .get("local_path")
                            .and_then(|v| v.as_str())
                            .map(PathBuf::from)
                    })
                    .flatten();
                out.push(WatchTarget {
                    workspace: ws.id,
                    store: c.name,
                    local_path,
                });
            }
        }
        out
    }
}

/// Start a recursive inotify watcher on `path` that signals `(ws, store)` on every
/// create/modify/remove (the downstream debounce coalesces a burst into one scan).
/// Mirrors `catalerum-calendar`'s `watch_dir`, but recursive and content-agnostic.
fn start_watcher(
    path: &Path,
    ws: WorkspaceId,
    store: String,
    tx: mpsc::UnboundedSender<StoreKey>,
) -> notify::Result<notify::RecommendedWatcher> {
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<NotifyEvent>| {
        let Ok(event) = res else { return };
        if matches!(
            event.kind,
            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
        ) {
            // Receiver gone => the worker is shutting down; ignore the send error.
            let _ = tx.send((ws, store.clone()));
        }
    })?;
    watcher.watch(path, RecursiveMode::Recursive)?;
    Ok(watcher)
}
