//! The durable sync worker (SOUL §6.2, §10).
//!
//! A [`SyncWorker`] runs a tokio loop that claims `sync_calendar` jobs from the
//! Postgres [`job_queue`](catalerum_store::JobQueueRepo) using `FOR UPDATE SKIP
//! LOCKED` (so many workers never grab the same row), runs
//! [`sync_connection`](crate::sync::sync_connection), then `complete`s the job
//! on success or `fail`s it with exponential backoff on error. This is the
//! single-pod dev dispatch path (Valkey disabled, SOUL §6.2); the same queue row
//! is what a Valkey Streams consumer would later claim.
//!
//! The same loop also runs a **reconciler** ([`SyncWorker::reconcile_once`]): a
//! worker that dies mid-job leaves its row `running` forever, so the loop
//! periodically re-drives leases left unacked past a visibility timeout (SOUL
//! §6.2) — a crash loses throughput, never work.
//!
//! Enqueue a sync with [`enqueue_sync`]; spawn the loop with [`spawn_worker`].

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use catalerum_core::id::{ConnectionId, WorkspaceId};
use catalerum_store::{JobRow, SecretStore, Store};

use crate::automation::{
    run_automation, AutomationContext, RunAutomationPayload, JOB_KIND_RUN_AUTOMATION,
};
use crate::collect::{
    run_collect_calendar_with, run_collect_email_with, CollectPayload, JOB_KIND_COLLECT_CALENDAR,
    JOB_KIND_COLLECT_EMAIL,
};
use crate::collect_sql::{run_collect_sql_with, JOB_KIND_COLLECT_SQL};
use crate::curate::{CurateContext, ExtractMemoriesPayload, JOB_KIND_EXTRACT_MEMORIES};
use crate::email::{ingest_email, IngestEmailPayload, JOB_KIND_INGEST_EMAIL};
use crate::embed::{
    EmbedContext, IngestMemoryPayload, IngestNotePayload, JOB_KIND_INGEST_MEMORY,
    JOB_KIND_INGEST_NOTE,
};
use crate::error::{IngestError, Result};
use crate::graph::{
    GraphContext, ProjectEventPayload, ProjectLinkPayload, ProjectNotePayload,
    JOB_KIND_PROJECT_EVENT, JOB_KIND_PROJECT_LINK, JOB_KIND_PROJECT_NOTE,
};
use crate::object::{IngestObjectPayload, ObjectIngestContext, JOB_KIND_INGEST_OBJECT};
use crate::profile::{run_profile_job, RunProfilePayload, JOB_KIND_RUN_PROFILE};
use crate::sync::sync_connection_with;

/// The `job_queue.kind` token for a calendar-sync job. Enqueue with this kind
/// (via [`enqueue_sync`]) and the [`SyncWorker`] will run it.
pub const JOB_KIND_SYNC_CALENDAR: &str = "sync_calendar";

/// The JSON payload of a [`JOB_KIND_SYNC_CALENDAR`] job: which connection to
/// sync, and optionally which workspace.
///
/// `connection_id` is always required. `workspace_id` is **optional**: when
/// present it is authoritative for the sync scope; when absent the worker falls
/// back to the job row's `workspace_id` column (which the API enqueues with
/// `Some(workspace)`), so a minimal `{ "connection_id": "…" }` payload — the
/// shape the API REST route produces — syncs end-to-end without the worker
/// rejecting it for a "missing field" (SOUL §6.2: the API and ingest are
/// decoupled across this kind+payload contract; the worker must accept what the
/// producer emits).
///
/// ```json
/// { "workspace_id": "<uuid>", "connection_id": "<uuid>" }   // explicit scope
/// { "connection_id": "<uuid>" }                              // scope from job row
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncCalendarPayload {
    /// The workspace that owns the connection. Optional on the wire: when
    /// omitted the worker resolves it from the job's `workspace_id` column.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    /// The connection to sync.
    pub connection_id: ConnectionId,
}

impl SyncCalendarPayload {
    /// Build a payload carrying an explicit workspace scope.
    #[must_use]
    pub fn new(workspace_id: WorkspaceId, connection_id: ConnectionId) -> Self {
        Self {
            workspace_id: Some(workspace_id),
            connection_id,
        }
    }

    /// Build a payload that defers its workspace scope to the job row's
    /// `workspace_id` column (the shape the API REST route enqueues).
    #[must_use]
    pub fn for_connection(connection_id: ConnectionId) -> Self {
        Self {
            workspace_id: None,
            connection_id,
        }
    }
}

/// Enqueue a durable `sync_calendar` job for `connection_id` in `workspace_id`
/// (SOUL §6.2). The worker claims it on its next poll; `run_after` is `now()` so
/// it is eligible immediately. Returns the enqueued job's id.
///
/// Idempotent at the data level: even if the same connection is enqueued twice,
/// each run is an idempotent sync (SOUL §3.4), so a duplicate job is at worst a
/// redundant no-op, never a duplicated calendar/event.
pub async fn enqueue_sync(
    store: &Store,
    workspace_id: WorkspaceId,
    connection_id: ConnectionId,
) -> Result<uuid::Uuid> {
    let payload = SyncCalendarPayload::new(workspace_id, connection_id);
    let job = store
        .job_queue()
        .enqueue(
            Some(workspace_id),
            JOB_KIND_SYNC_CALENDAR,
            serde_json::to_value(payload)?,
            None,
        )
        .await?;
    debug!(job = %job.id, %connection_id, "enqueued sync_calendar job");
    Ok(job.id)
}

/// Tuning knobs for a [`SyncWorker`].
#[derive(Clone, Debug)]
pub struct WorkerConfig {
    /// A label recorded as the job's `locked_by` lease holder. Defaults to
    /// `"ingest-worker"` (override per pod/host for observability).
    pub worker_id: String,
    /// How long to sleep when the queue is empty before polling again.
    pub idle_poll: Duration,
    /// Max attempts before a job becomes terminally `failed`.
    pub max_attempts: i32,
    /// Base backoff; a failed attempt retries after
    /// `backoff_base * 2^(attempts-1)` (SOUL §6.2).
    pub backoff_base: Duration,
    /// How long a claimed (`running`) job may hold its lease before the
    /// reconciler considers it orphaned by a crashed worker and re-drives it
    /// (SOUL §6.2). Must exceed the longest expected job runtime, or a job that
    /// is merely slow gets reclaimed and run twice.
    pub visibility_timeout: Duration,
    /// How often the worker runs the stale-lease reconciler
    /// ([`SyncWorker::reconcile_once`]).
    pub reconcile_every: Duration,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            worker_id: "ingest-worker".to_string(),
            idle_poll: Duration::from_secs(2),
            max_attempts: 5,
            backoff_base: Duration::from_secs(10),
            // 5 min comfortably exceeds a calendar sync; 1 min reconcile cadence
            // keeps orphaned jobs from sitting stuck for long after a crash.
            visibility_timeout: Duration::from_secs(300),
            reconcile_every: Duration::from_secs(60),
        }
    }
}

/// A polling worker over the durable `job_queue` (SOUL §6.2/§10).
#[derive(Clone)]
pub struct SyncWorker {
    store: Store,
    config: WorkerConfig,
    /// Services for `ingest_note` jobs (SOUL §6.4/§10). `None` → this worker
    /// cannot embed, so an `ingest_note` job it claims fails (and re-queues)
    /// until a worker with an embed context runs it.
    embed: Option<EmbedContext>,
    /// Services for `project_note` jobs (SOUL §6.3/§10). `None` → this worker
    /// cannot project to the graph; the job re-queues for a graph-capable peer.
    graph: Option<GraphContext>,
    /// Services for `extract_memories` jobs (SOUL §22). `None` → this worker
    /// cannot extract; the job re-queues for a curation-capable peer.
    curate: Option<CurateContext>,
    /// Services for `run_automation` jobs (SOUL §11). `None` → this worker has no
    /// action runner, so the job re-queues for an automation-capable peer.
    automation: Option<AutomationContext>,
    /// Services for `ingest_object` jobs (SOUL §9/§10) — the storage backend to
    /// read object bytes. `None` → this worker can't read objects; the job
    /// re-queues for a storage-capable peer. The optional embed context (above)
    /// is reused for the derived Qdrant embed when present.
    objects: Option<ObjectIngestContext>,
    /// The encrypted secret store (SOUL §13), when `[secrets].master_key` is set.
    /// Needed only to build the OAuth-backed **Google** calendar provider (whose
    /// tokens are sealed behind the connection's `credential_ref`); every other
    /// provider ignores it. `None` → a Google calendar sync/collect job fails
    /// closed with a clear error (no plaintext-token fallback).
    secrets: Option<Arc<SecretStore>>,
    /// The coordination bus (SOUL §6.6/§16 M7), when attached. Serializes collect
    /// jobs **per source across pods**: the scheduler's fire lock only single-fires
    /// the *enqueue*, so under a backlog (or a collect outliving its cadence) several
    /// queued collect jobs for one source can be claimed concurrently by different
    /// pods — each would then see the same uncommitted ledger and fan out duplicate
    /// per-item runs. A held per-source mutex makes the overlapping job *skip*
    /// (harmless: the cursor-based next occurrence re-collects). `None` → no
    /// cross-pod serialization (single-pod dev is inherently serial per worker).
    bus: Option<catalerum_bus::Bus>,
}

// `SecretStore` holds an opaque cipher key and isn't `Debug`; skip it (never log a
// credential) while keeping the worker `{:?}`-printable.
impl std::fmt::Debug for SyncWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncWorker")
            .field("config", &self.config)
            .field("has_embed", &self.embed.is_some())
            .field("has_graph", &self.graph.is_some())
            .field("has_curate", &self.curate.is_some())
            .field("has_automation", &self.automation.is_some())
            .field("has_objects", &self.objects.is_some())
            .field("has_secrets", &self.secrets.is_some())
            .field("has_bus", &self.bus.is_some())
            .finish_non_exhaustive()
    }
}

/// TTL on the per-source collect mutex (SOUL §16 M7). Generous — a collect runs
/// its items inline and one item can be a whole LLM run — and extended by a
/// background refresher while the collect runs; a crashed holder's lock simply
/// TTL-expires.
const COLLECT_SOURCE_LOCK_TTL: Duration = Duration::from_secs(300);
/// How often the holder's refresher extends the mutex (well inside the TTL).
const COLLECT_SOURCE_LOCK_REFRESH: Duration = Duration::from_secs(60);

/// A held per-source collect mutex plus its background TTL refresher.
struct CollectSourceLock {
    bus: catalerum_bus::Bus,
    guard: catalerum_bus::LockGuard,
    refresher: JoinHandle<()>,
}

impl CollectSourceLock {
    /// Stop refreshing and free the mutex (best-effort; the TTL is the backstop).
    async fn release(self) {
        self.refresher.abort();
        let _ = self.bus.lock().release(&self.guard).await;
    }
}

/// Whether a collect job may proceed, and under which (optional) held mutex.
enum CollectGate {
    Proceed(Option<CollectSourceLock>),
    /// Another pod is mid-collect on this source — skip; the cursor-based next
    /// occurrence re-collects anything this job would have seen (loss-free).
    Skip,
}

/// The per-source mutex resource for a collect job: the trigger's `connection`
/// when present — the committed ledger the mutex protects lives on the
/// connection's `sync_token`, and two automations sharing a connection share
/// that ledger — else the automation id.
fn collect_source_resource(payload: &CollectPayload) -> String {
    let connection = payload
        .trigger
        .get("connection")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    match connection {
        Some(c) => format!("collect-src:{c}"),
        None => format!("collect-src:{}", payload.automation_id),
    }
}

impl SyncWorker {
    /// Build a worker with the default [`WorkerConfig`] and no embed context.
    #[must_use]
    pub fn new(store: Store) -> Self {
        Self {
            store,
            config: WorkerConfig::default(),
            embed: None,
            graph: None,
            curate: None,
            automation: None,
            objects: None,
            secrets: None,
            bus: None,
        }
    }

    /// Build a worker with an explicit [`WorkerConfig`].
    #[must_use]
    pub fn with_config(store: Store, config: WorkerConfig) -> Self {
        Self {
            store,
            config,
            embed: None,
            graph: None,
            curate: None,
            automation: None,
            objects: None,
            secrets: None,
            bus: None,
        }
    }

    /// Attach an [`EmbedContext`] so this worker also handles `ingest_note` jobs
    /// (embed a note's chunks into Qdrant, SOUL §6.4/§10).
    #[must_use]
    pub fn with_embed_context(mut self, embed: EmbedContext) -> Self {
        self.embed = Some(embed);
        self
    }

    /// Attach a [`GraphContext`] so this worker also handles `project_note` jobs
    /// (project a note into the Neo4j graph, SOUL §6.3/§10).
    #[must_use]
    pub fn with_graph_context(mut self, graph: GraphContext) -> Self {
        self.graph = Some(graph);
        self
    }

    /// Attach a [`CurateContext`] so this worker also handles `extract_memories`
    /// jobs (mine a conversation for durable facts, SOUL §22).
    #[must_use]
    pub fn with_curate_context(mut self, curate: CurateContext) -> Self {
        self.curate = Some(curate);
        self
    }

    /// Attach an [`AutomationContext`] so this worker also handles `run_automation`
    /// jobs (run a matched automation's actions, SOUL §11).
    #[must_use]
    pub fn with_automation_context(mut self, automation: AutomationContext) -> Self {
        self.automation = Some(automation);
        self
    }

    /// Attach an [`ObjectIngestContext`] so this worker also handles
    /// `ingest_object` jobs (extract a stored object's text into `documents` +
    /// embed it, SOUL §9/§10).
    #[must_use]
    pub fn with_object_context(mut self, objects: ObjectIngestContext) -> Self {
        self.objects = Some(objects);
        self
    }

    /// Attach the encrypted secret store (SOUL §13) so this worker can build the
    /// OAuth-backed **Google** calendar provider for `sync_calendar` /
    /// `collect_calendar` jobs (the tokens are sealed behind the connection's
    /// `credential_ref`). `None` leaves Google sources failing closed; every other
    /// calendar backend is unaffected.
    #[must_use]
    pub fn with_secret_store(mut self, secrets: Option<Arc<SecretStore>>) -> Self {
        self.secrets = secrets;
        self
    }

    /// Attach the coordination bus (SOUL §6.6/§16 M7) so collect jobs are
    /// serialized **per source across pods** (see the field docs). Without it a
    /// backlog of queued collects for one source can run concurrently on several
    /// pods and fan out duplicate per-item runs.
    #[must_use]
    pub fn with_bus(mut self, bus: catalerum_bus::Bus) -> Self {
        self.bus = Some(bus);
        self
    }

    /// Take the cross-pod per-source collect mutex (SOUL §16 M7). The scheduler's
    /// fire lock only single-fires the *enqueue*; under a backlog several queued
    /// collect jobs for one source can be claimed concurrently by different pods,
    /// each seeing the same uncommitted ledger and fanning out duplicate per-item
    /// runs. [`CollectGate::Skip`] when another pod holds the source. No bus →
    /// proceed unserialized (single-pod dev); a bus *error* also proceeds, with a
    /// warning — the bus is a coordination hint, never a correctness oracle
    /// (§6.6), and the scheduler's enqueue-side lock already thins the herd.
    async fn collect_gate(&self, payload: &CollectPayload) -> CollectGate {
        let Some(bus) = &self.bus else {
            return CollectGate::Proceed(None);
        };
        let resource = collect_source_resource(payload);
        match bus
            .lock()
            .try_acquire(&resource, COLLECT_SOURCE_LOCK_TTL)
            .await
        {
            Ok(Some(guard)) => {
                let refresher = tokio::spawn({
                    let bus = bus.clone();
                    let guard = guard.clone();
                    async move {
                        let mut tick = tokio::time::interval(COLLECT_SOURCE_LOCK_REFRESH);
                        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                        loop {
                            tick.tick().await;
                            // A failed refresh means the lock lapsed under us (and
                            // may be re-held elsewhere); stop extending a lock we
                            // no longer own.
                            if !bus
                                .lock()
                                .refresh(&guard, COLLECT_SOURCE_LOCK_TTL)
                                .await
                                .unwrap_or(false)
                            {
                                break;
                            }
                        }
                    }
                });
                CollectGate::Proceed(Some(CollectSourceLock {
                    bus: bus.clone(),
                    guard,
                    refresher,
                }))
            }
            Ok(None) => CollectGate::Skip,
            Err(e) => {
                warn!(error = %e, %resource,
                    "collect source-lock unavailable; proceeding unserialized");
                CollectGate::Proceed(None)
            }
        }
    }

    /// Spawn this worker's [`run`](Self::run) loop as a detached background task.
    #[must_use]
    pub fn spawn(self) -> JoinHandle<()> {
        tokio::spawn(self.run())
    }

    /// Run the claim → process → complete/fail loop forever (until the task is
    /// aborted). Drains all eligible jobs, then sleeps `idle_poll` when the
    /// queue is empty. Errors are logged and retried via the queue; the loop
    /// itself never exits on a job failure (only a queue/DB outage backs off).
    pub async fn run(self) {
        info!(worker = %self.config.worker_id, "sync worker started");
        // Reclaim anything a previous worker left orphaned (crashed mid-job)
        // before we start draining, then re-run the reconciler periodically
        // (SOUL §6.2).
        if let Err(e) = self.reconcile_once().await {
            warn!(worker = %self.config.worker_id, error = %e, "startup reconcile failed; will retry");
        }
        let mut last_reconcile = tokio::time::Instant::now();
        loop {
            if last_reconcile.elapsed() >= self.config.reconcile_every {
                if let Err(e) = self.reconcile_once().await {
                    warn!(worker = %self.config.worker_id, error = %e, "reconcile failed; will retry");
                }
                last_reconcile = tokio::time::Instant::now();
            }
            match self.poll_once().await {
                Ok(true) => {
                    // Claimed and processed a job; immediately try for another
                    // so a backlog drains without idle sleeps.
                }
                Ok(false) => {
                    tokio::time::sleep(self.config.idle_poll).await;
                }
                Err(e) => {
                    // A queue/DB-level failure (claiming, completing). Back off
                    // and retry rather than spin.
                    warn!(worker = %self.config.worker_id, error = %e, "worker poll failed; backing off");
                    tokio::time::sleep(self.config.idle_poll).await;
                }
            }
        }
    }

    /// Claim at most one job and process it. Returns `Ok(true)` if a job was
    /// claimed (regardless of whether the sync itself succeeded — a sync error
    /// is recorded on the job, not propagated), `Ok(false)` if the queue was
    /// empty, and `Err` only on a queue/DB-level failure.
    pub async fn poll_once(&self) -> Result<bool> {
        let Some(job) = self
            .store
            .job_queue()
            .dequeue_one(&self.config.worker_id)
            .await?
        else {
            return Ok(false);
        };

        self.process(job).await;
        Ok(true)
    }

    /// Run the stale-lease reconciler once (SOUL §6.2): re-drive any job whose
    /// worker crashed mid-run, leaving it `running` past
    /// [`WorkerConfig::visibility_timeout`]. Returns the number of jobs
    /// reclaimed. The reclaim is a single atomic `UPDATE`, so it is safe to run
    /// from every worker concurrently. `Err` only on a queue/DB-level failure.
    pub async fn reconcile_once(&self) -> Result<u64> {
        let reclaimed = self
            .store
            .job_queue()
            .reclaim_stale(self.config.visibility_timeout, self.config.max_attempts)
            .await?;
        if reclaimed > 0 {
            info!(
                worker = %self.config.worker_id,
                reclaimed,
                "reconciled stale job leases (re-driven after a crashed worker)"
            );
        }
        Ok(reclaimed)
    }

    /// Run one claimed job to completion, recording success/failure on the queue
    /// row. A failure re-queues with backoff up to `max_attempts`, then becomes
    /// terminal `failed` (SOUL §6.2).
    async fn process(&self, job: JobRow) {
        let job_id = job.id;
        let result = self.dispatch(&job).await;
        match result {
            Ok(()) => {
                if let Err(e) = self.store.job_queue().complete(job_id).await {
                    error!(job = %job_id, error = %e, "failed to mark job done");
                }
            }
            Err(e) => {
                warn!(job = %job_id, kind = %job.kind, error = %e, "job failed; will retry per backoff");
                if let Err(fe) = self
                    .store
                    .job_queue()
                    .fail(
                        job_id,
                        &e.to_string(),
                        self.config.max_attempts,
                        self.config.backoff_base,
                    )
                    .await
                {
                    error!(job = %job_id, error = %fe, "failed to record job failure");
                }
            }
        }
    }

    /// Route a job by `kind` to its handler. Only `sync_calendar` is handled
    /// here; other kinds (graph projection, embedding) arrive in later
    /// milestones and surface as [`IngestError::UnknownKind`] so they are not
    /// silently dropped.
    async fn dispatch(&self, job: &JobRow) -> Result<()> {
        match job.kind.as_str() {
            JOB_KIND_SYNC_CALENDAR => {
                let payload: SyncCalendarPayload = serde_json::from_value(job.payload().clone())?;
                // Resolve the workspace scope: the payload wins when it carries
                // one, else fall back to the job row's `workspace_id` column
                // (the API enqueues sync jobs with `Some(workspace)`). A job
                // with neither is unscoped and cannot be safely run.
                let workspace_id = payload
                    .workspace_id
                    .or_else(|| job.workspace_id())
                    .ok_or_else(|| {
                        IngestError::invalid_job(format!(
                            "sync_calendar job {} has no workspace_id (absent in payload and job row)",
                            job.id
                        ))
                    })?;
                let report = sync_connection_with(
                    &self.store,
                    workspace_id,
                    payload.connection_id,
                    self.secrets.as_ref(),
                )
                .await?;
                info!(
                    job = %job.id,
                    workspace = %workspace_id,
                    connection = %payload.connection_id,
                    calendars = report.calendars,
                    upserted = report.events_upserted,
                    deleted = report.events_deleted,
                    "sync_calendar done"
                );
                Ok(())
            }
            JOB_KIND_INGEST_NOTE => {
                let payload: IngestNotePayload = serde_json::from_value(job.payload().clone())?;
                let workspace_id = payload
                    .workspace_id
                    .or_else(|| job.workspace_id())
                    .ok_or_else(|| {
                        IngestError::invalid_job(format!(
                            "ingest_note job {} has no workspace_id (absent in payload and job row)",
                            job.id
                        ))
                    })?;
                // Embedding requires an embed context. Without one this worker
                // cannot run the job; surface it as a retryable failure (a peer
                // worker that *is* embed-capable will pick it up) rather than
                // silently dropping it.
                let Some(embed) = &self.embed else {
                    return Err(IngestError::invalid_job(format!(
                        "ingest_note job {} needs an embed context; this worker has none",
                        job.id
                    )));
                };
                let report = embed
                    .ingest_note(&self.store, workspace_id, payload.note_id)
                    .await?;
                info!(
                    job = %job.id,
                    workspace = %workspace_id,
                    note = %payload.note_id,
                    document = ?report.document_id,
                    chunks = report.chunks,
                    purged = report.document_id.is_none(),
                    "ingest_note done"
                );
                Ok(())
            }
            JOB_KIND_INGEST_MEMORY => {
                let payload: IngestMemoryPayload = serde_json::from_value(job.payload().clone())?;
                let workspace_id = payload
                    .workspace_id
                    .or_else(|| job.workspace_id())
                    .ok_or_else(|| {
                        IngestError::invalid_job(format!(
                            "ingest_memory job {} has no workspace_id (absent in payload and job row)",
                            job.id
                        ))
                    })?;
                let Some(embed) = &self.embed else {
                    return Err(IngestError::invalid_job(format!(
                        "ingest_memory job {} needs an embed context; this worker has none",
                        job.id
                    )));
                };
                let report = embed
                    .ingest_memory(&self.store, workspace_id, payload.memory_id)
                    .await?;
                info!(
                    job = %job.id,
                    workspace = %workspace_id,
                    memory = %payload.memory_id,
                    chunks = report.chunks,
                    purged = report.document_id.is_none(),
                    "ingest_memory done"
                );
                Ok(())
            }
            JOB_KIND_PROJECT_NOTE => {
                let payload: ProjectNotePayload = serde_json::from_value(job.payload().clone())?;
                let workspace_id = payload
                    .workspace_id
                    .or_else(|| job.workspace_id())
                    .ok_or_else(|| {
                        IngestError::invalid_job(format!(
                            "project_note job {} has no workspace_id (absent in payload and job row)",
                            job.id
                        ))
                    })?;
                let Some(graph) = &self.graph else {
                    return Err(IngestError::invalid_job(format!(
                        "project_note job {} needs a graph context; this worker has none",
                        job.id
                    )));
                };
                let report = graph
                    .project_note(&self.store, workspace_id, payload.note_id)
                    .await?;
                info!(
                    job = %job.id,
                    workspace = %workspace_id,
                    note = %payload.note_id,
                    topics = report.topics,
                    purged = report.purged,
                    "project_note done"
                );
                Ok(())
            }
            JOB_KIND_PROJECT_EVENT => {
                let payload: ProjectEventPayload = serde_json::from_value(job.payload().clone())?;
                let workspace_id = payload
                    .workspace_id
                    .or_else(|| job.workspace_id())
                    .ok_or_else(|| {
                        IngestError::invalid_job(format!(
                            "project_event job {} has no workspace_id (absent in payload and job row)",
                            job.id
                        ))
                    })?;
                let Some(graph) = &self.graph else {
                    return Err(IngestError::invalid_job(format!(
                        "project_event job {} needs a graph context; this worker has none",
                        job.id
                    )));
                };
                let purged = graph
                    .project_event(&self.store, workspace_id, payload.event_id)
                    .await?;
                info!(
                    job = %job.id,
                    workspace = %workspace_id,
                    event = %payload.event_id,
                    purged,
                    "project_event done"
                );
                Ok(())
            }
            JOB_KIND_PROJECT_LINK => {
                let payload: ProjectLinkPayload = serde_json::from_value(job.payload().clone())?;
                let workspace_id = payload
                    .workspace_id
                    .or_else(|| job.workspace_id())
                    .ok_or_else(|| {
                        IngestError::invalid_job(format!(
                            "project_link job {} has no workspace_id (absent in payload and job row)",
                            job.id
                        ))
                    })?;
                let Some(graph) = &self.graph else {
                    return Err(IngestError::invalid_job(format!(
                        "project_link job {} needs a graph context; this worker has none",
                        job.id
                    )));
                };
                let purged = graph
                    .project_link(&self.store, workspace_id, payload.link_id)
                    .await?;
                info!(
                    job = %job.id,
                    workspace = %workspace_id,
                    link = %payload.link_id,
                    purged,
                    "project_link done"
                );
                Ok(())
            }
            JOB_KIND_EXTRACT_MEMORIES => {
                let payload: ExtractMemoriesPayload =
                    serde_json::from_value(job.payload().clone())?;
                let workspace_id = payload
                    .workspace_id
                    .or_else(|| job.workspace_id())
                    .ok_or_else(|| {
                        IngestError::invalid_job(format!(
                            "extract_memories job {} has no workspace_id (absent in payload and job row)",
                            job.id
                        ))
                    })?;
                let Some(curate) = &self.curate else {
                    return Err(IngestError::invalid_job(format!(
                        "extract_memories job {} needs a curate context; this worker has none",
                        job.id
                    )));
                };
                let report = curate
                    .extract(
                        &self.store,
                        // Reuse this worker's embed context (when present) so the
                        // dedup seam's similarity layer runs on the auto-store path.
                        self.embed.as_ref(),
                        workspace_id,
                        payload.conversation_id,
                        payload.user_id,
                    )
                    .await?;
                info!(
                    job = %job.id,
                    workspace = %workspace_id,
                    conversation = %payload.conversation_id,
                    proposed = report.proposed,
                    created = report.created,
                    "extract_memories done"
                );
                Ok(())
            }
            JOB_KIND_INGEST_OBJECT => {
                let payload: IngestObjectPayload = serde_json::from_value(job.payload().clone())?;
                let workspace_id = payload
                    .workspace_id
                    .or_else(|| job.workspace_id())
                    .ok_or_else(|| {
                        IngestError::invalid_job(format!(
                            "ingest_object job {} has no workspace_id (absent in payload and job row)",
                            job.id
                        ))
                    })?;
                // Reading object bytes needs a storage backend. Without one this
                // worker can't run the job; surface it as a retryable failure (a
                // storage-capable peer picks it up) rather than dropping it.
                let Some(objects) = &self.objects else {
                    return Err(IngestError::invalid_job(format!(
                        "ingest_object job {} needs an object context; this worker has none",
                        job.id
                    )));
                };
                let report = objects
                    .ingest(
                        &self.store,
                        self.embed.as_ref(),
                        workspace_id,
                        payload.object_id,
                    )
                    .await?;
                info!(
                    job = %job.id,
                    workspace = %workspace_id,
                    object = %payload.object_id,
                    document = ?report.document_id,
                    chunks = report.chunks,
                    text_bytes = report.text_bytes,
                    "ingest_object done"
                );
                Ok(())
            }
            JOB_KIND_INGEST_EMAIL => {
                let payload: IngestEmailPayload = serde_json::from_value(job.payload().clone())?;
                let workspace_id = payload
                    .workspace_id
                    .or_else(|| job.workspace_id())
                    .ok_or_else(|| {
                        IngestError::invalid_job(format!(
                            "ingest_email job {} has no workspace_id (absent in payload and job row)",
                            job.id
                        ))
                    })?;
                // Email text is already in Postgres, so the document-catalogue
                // step runs on any worker; the optional embed context layers the
                // Qdrant projection on when present (SOUL §28/§10).
                let report = ingest_email(
                    &self.store,
                    self.embed.as_ref(),
                    workspace_id,
                    payload.email_id,
                )
                .await?;
                info!(
                    job = %job.id,
                    workspace = %workspace_id,
                    email = %payload.email_id,
                    document = ?report.document_id,
                    chunks = report.chunks,
                    "ingest_email done"
                );
                Ok(())
            }
            JOB_KIND_RUN_AUTOMATION => {
                let payload: RunAutomationPayload = serde_json::from_value(job.payload().clone())?;
                let workspace_id = payload
                    .workspace_id
                    .or_else(|| job.workspace_id())
                    .ok_or_else(|| {
                        IngestError::invalid_job(format!(
                            "run_automation job {} has no workspace_id (absent in payload and job row)",
                            job.id
                        ))
                    })?;
                let Some(automation) = &self.automation else {
                    return Err(IngestError::invalid_job(format!(
                        "run_automation job {} needs an automation context; this worker has none",
                        job.id
                    )));
                };
                let run_id = run_automation(
                    &self.store,
                    automation.runner().as_ref(),
                    automation.code().as_ref(),
                    workspace_id,
                    payload.automation_id,
                    payload.trigger.clone(),
                    job.id,
                )
                .await?;
                info!(
                    job = %job.id,
                    workspace = %workspace_id,
                    automation = %payload.automation_id,
                    run = ?run_id,
                    skipped = run_id.is_none(),
                    "run_automation done"
                );
                Ok(())
            }
            JOB_KIND_COLLECT_EMAIL => {
                let payload: CollectPayload = serde_json::from_value(job.payload().clone())?;
                let workspace_id = payload
                    .workspace_id
                    .or_else(|| job.workspace_id())
                    .ok_or_else(|| {
                        IngestError::invalid_job(format!(
                            "collect_email job {} has no workspace_id (absent in payload and job row)",
                            job.id
                        ))
                    })?;
                // Per-item runs need the action runner; without one this worker
                // can't fan out, so re-queue for an automation-capable peer.
                let Some(automation) = &self.automation else {
                    return Err(IngestError::invalid_job(format!(
                        "collect_email job {} needs an automation context; this worker has none",
                        job.id
                    )));
                };
                // Serialize per source across pods (SOUL §16 M7): an overlapping
                // collect would re-run the same uncommitted items.
                let held = match self.collect_gate(&payload).await {
                    CollectGate::Proceed(h) => h,
                    CollectGate::Skip => {
                        info!(job = %job.id, workspace = %workspace_id,
                            "another pod is collecting this source; skipping (next occurrence re-collects)");
                        return Ok(());
                    }
                };
                let result = run_collect_email_with(
                    &self.store,
                    automation,
                    workspace_id,
                    &payload,
                    self.secrets.as_ref(),
                )
                .await;
                if let Some(held) = held {
                    held.release().await;
                }
                let report = result?;
                info!(
                    job = %job.id,
                    workspace = %workspace_id,
                    sources = report.sources,
                    fired = report.runs_fired,
                    committed = report.committed,
                    deleted = report.deleted,
                    "collect_email done"
                );
                Ok(())
            }
            JOB_KIND_COLLECT_CALENDAR => {
                let payload: CollectPayload = serde_json::from_value(job.payload().clone())?;
                let workspace_id = payload
                    .workspace_id
                    .or_else(|| job.workspace_id())
                    .ok_or_else(|| {
                        IngestError::invalid_job(format!(
                            "collect_calendar job {} has no workspace_id (absent in payload and job row)",
                            job.id
                        ))
                    })?;
                let Some(automation) = &self.automation else {
                    return Err(IngestError::invalid_job(format!(
                        "collect_calendar job {} needs an automation context; this worker has none",
                        job.id
                    )));
                };
                // Serialize per source across pods (SOUL §16 M7), exactly like
                // the email arm above.
                let held = match self.collect_gate(&payload).await {
                    CollectGate::Proceed(h) => h,
                    CollectGate::Skip => {
                        info!(job = %job.id, workspace = %workspace_id,
                            "another pod is collecting this source; skipping (next occurrence re-collects)");
                        return Ok(());
                    }
                };
                let result = run_collect_calendar_with(
                    &self.store,
                    automation,
                    workspace_id,
                    &payload,
                    self.secrets.as_ref(),
                )
                .await;
                if let Some(held) = held {
                    held.release().await;
                }
                let report = result?;
                info!(
                    job = %job.id,
                    workspace = %workspace_id,
                    sources = report.sources,
                    fired = report.runs_fired,
                    committed = report.committed,
                    deleted = report.deleted,
                    "collect_calendar done"
                );
                Ok(())
            }
            JOB_KIND_COLLECT_SQL => {
                let payload: CollectPayload = serde_json::from_value(job.payload().clone())?;
                let workspace_id = payload
                    .workspace_id
                    .or_else(|| job.workspace_id())
                    .ok_or_else(|| {
                        IngestError::invalid_job(format!(
                            "collect_sql job {} has no workspace_id (absent in payload and job row)",
                            job.id
                        ))
                    })?;
                let Some(automation) = &self.automation else {
                    return Err(IngestError::invalid_job(format!(
                        "collect_sql job {} needs an automation context; this worker has none",
                        job.id
                    )));
                };
                // Serialize per source across pods (SOUL §16 M7), exactly like
                // the email arm above — the shared per-connection ledger is what
                // the mutex protects.
                let held = match self.collect_gate(&payload).await {
                    CollectGate::Proceed(h) => h,
                    CollectGate::Skip => {
                        info!(job = %job.id, workspace = %workspace_id,
                            "another pod is collecting this source; skipping (next occurrence re-collects)");
                        return Ok(());
                    }
                };
                let result = run_collect_sql_with(
                    &self.store,
                    automation,
                    workspace_id,
                    &payload,
                    self.secrets.as_ref(),
                )
                .await;
                if let Some(held) = held {
                    held.release().await;
                }
                let report = result?;
                info!(
                    job = %job.id,
                    workspace = %workspace_id,
                    tables = report.sources,
                    fired = report.runs_fired,
                    committed = report.committed,
                    "collect_sql done"
                );
                Ok(())
            }
            JOB_KIND_RUN_PROFILE => {
                let payload: RunProfilePayload = serde_json::from_value(job.payload().clone())?;
                let workspace_id = payload
                    .workspace_id
                    .or_else(|| job.workspace_id())
                    .ok_or_else(|| {
                        IngestError::invalid_job(format!(
                            "run_profile job {} has no workspace_id (absent in payload and job row)",
                            job.id
                        ))
                    })?;
                let Some(automation) = &self.automation else {
                    return Err(IngestError::invalid_job(format!(
                        "run_profile job {} needs an automation context (its ActionRunner); this worker has none",
                        job.id
                    )));
                };
                run_profile_job(
                    &self.store,
                    automation.runner().as_ref(),
                    workspace_id,
                    payload,
                )
                .await?;
                Ok(())
            }
            other => Err(IngestError::UnknownKind(other.to_string())),
        }
    }
}

/// Spawn the [`SyncWorker`] loop as a detached background tokio task and return
/// its [`JoinHandle`] (SOUL §10). The binary calls this after the services
/// connect; the task runs alongside serving and never blocks it.
///
/// ```no_run
/// # async fn run(store: catalerum_store::Store) {
/// let handle = catalerum_ingest::spawn_worker(store);
/// // ... serve the API; the worker drains `sync_calendar` jobs in the background.
/// let _ = handle;
/// # }
/// ```
#[must_use]
pub fn spawn_worker(store: Store) -> JoinHandle<()> {
    spawn_worker_with(store, WorkerConfig::default())
}

/// Spawn the worker with an explicit [`WorkerConfig`].
#[must_use]
pub fn spawn_worker_with(store: Store, config: WorkerConfig) -> JoinHandle<()> {
    tokio::spawn(SyncWorker::with_config(store, config).run())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_round_trips_through_json() {
        let p = SyncCalendarPayload::new(WorkspaceId::new(), ConnectionId::new());
        let json = serde_json::to_value(p).unwrap();
        // Stable, documented shape.
        assert!(json.get("workspace_id").is_some());
        assert!(json.get("connection_id").is_some());
        let back: SyncCalendarPayload = serde_json::from_value(json).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn payload_accepts_connection_only_shape() {
        // The shape the API REST route enqueues: `{ "connection_id": "…" }`
        // with no `workspace_id`. The worker must deserialize it (the scope is
        // resolved from the job row's `workspace_id` column), not reject it for
        // a missing field — this is the contract regression guard.
        let conn = ConnectionId::new();
        let json = serde_json::json!({ "connection_id": conn });
        let p: SyncCalendarPayload = serde_json::from_value(json).unwrap();
        assert_eq!(p.workspace_id, None);
        assert_eq!(p.connection_id, conn);

        // And the constructor for that shape round-trips with no workspace key.
        let built = SyncCalendarPayload::for_connection(conn);
        assert_eq!(built, p);
        let reser = serde_json::to_value(built).unwrap();
        assert!(reser.get("workspace_id").is_none());
    }

    #[test]
    fn job_kind_token_is_stable() {
        assert_eq!(JOB_KIND_SYNC_CALENDAR, "sync_calendar");
    }

    /// The cross-pod per-source collect mutex (SOUL §16 M7): while one worker
    /// holds a source, a second worker's gate says Skip; after release it
    /// proceeds again. Two automations naming the same connection contend on the
    /// same resource (the ledger they'd race lives on the connection).
    #[tokio::test]
    async fn collect_gate_serializes_a_source_across_workers() {
        let bus = catalerum_bus::Bus::in_process();
        let store_url = std::env::var("CATALERUM_TEST_DATABASE_URL")
            .or_else(|_| std::env::var("DATABASE_URL"))
            .ok();
        // The gate itself never touches the store, but SyncWorker::new needs one;
        // skip when no test DB is reachable.
        let Some(url) = store_url else {
            eprintln!("skipping collect_gate_serializes_a_source_across_workers: set CATALERUM_TEST_DATABASE_URL");
            return;
        };
        let Ok(store) = Store::connect(&url).await else {
            eprintln!(
                "skipping collect_gate_serializes_a_source_across_workers: test DB unreachable"
            );
            return;
        };
        let a = SyncWorker::new(store.clone()).with_bus(bus.clone());
        let b = SyncWorker::new(store).with_bus(bus);
        let conn = uuid::Uuid::new_v4();
        let payload = |automation: uuid::Uuid| CollectPayload {
            workspace_id: None,
            automation_id: automation.into(),
            trigger: serde_json::json!({ "kind": "collect_email", "connection": conn.to_string() }),
        };
        let p1 = payload(uuid::Uuid::new_v4());
        let p2 = payload(uuid::Uuid::new_v4());

        let held = match a.collect_gate(&p1).await {
            CollectGate::Proceed(Some(h)) => h,
            _ => panic!("first gate must proceed holding the source"),
        };
        // A second worker — and even a DIFFERENT automation on the same
        // connection — skips while the source is held.
        assert!(matches!(b.collect_gate(&p2).await, CollectGate::Skip));
        held.release().await;
        match b.collect_gate(&p2).await {
            CollectGate::Proceed(Some(h)) => h.release().await,
            _ => panic!("gate must proceed again after release"),
        }
    }

    #[test]
    fn collect_source_resource_prefers_the_connection() {
        let automation = uuid::Uuid::new_v4();
        let with_conn = CollectPayload {
            workspace_id: None,
            automation_id: automation.into(),
            trigger: serde_json::json!({ "connection": "abc-123" }),
        };
        assert_eq!(collect_source_resource(&with_conn), "collect-src:abc-123");
        let without = CollectPayload {
            workspace_id: None,
            automation_id: automation.into(),
            trigger: serde_json::json!({}),
        };
        assert_eq!(
            collect_source_resource(&without),
            format!("collect-src:{automation}")
        );
    }

    #[test]
    fn default_worker_config_is_sane() {
        let c = WorkerConfig::default();
        assert!(c.max_attempts >= 1);
        assert!(c.backoff_base > Duration::ZERO);
        assert_eq!(c.worker_id, "ingest-worker");
        assert!(c.reconcile_every > Duration::ZERO);
        // The visibility timeout must exceed a single poll cadence, or a job
        // could be reclaimed before its worker even gets a chance to finish.
        assert!(c.visibility_timeout > c.idle_poll);
    }
}
