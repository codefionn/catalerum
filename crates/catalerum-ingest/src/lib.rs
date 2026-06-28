//! catalerum-ingest — sync orchestration (SOUL §10).
//!
//! Per connection, on schedule or by an enqueued job:
//! `pull (cursor)` → `normalize` → `upsert Postgres` → (later: enqueue graph
//! projection + chunk/embed/upsert-to-Qdrant). Every step is idempotent and
//! resumable from the durable `job_queue` (SOUL §3.4/§6.2).
//!
//! # What's here (M2 — calendar ingest)
//! - [`sync_connection`] — the orchestration entry point: build the matching
//!   [`CalendarProvider`](catalerum_core::provider::CalendarProvider) from a
//!   connection's stored `config`, upsert its calendars, incrementally sync each
//!   calendar's events into Postgres, and persist the per-calendar cursors. A
//!   second run with no source changes is a no-op.
//! - [`SyncWorker`] / [`spawn_worker`] — a tokio loop that claims
//!   [`JOB_KIND_SYNC_CALENDAR`] jobs from the `job_queue` (`FOR UPDATE SKIP
//!   LOCKED`), runs the sync, and completes / fails-with-backoff.
//! - [`enqueue_sync`] — enqueue a durable sync job for a connection.
//!
//! # Job contract
//! Kind: [`JOB_KIND_SYNC_CALENDAR`] (`"sync_calendar"`). Payload:
//! [`SyncCalendarPayload`] — `{ "workspace_id": "<uuid>", "connection_id":
//! "<uuid>" }`.
//!
//! ```no_run
//! # async fn run(store: catalerum_store::Store,
//! #              ws: catalerum_core::id::WorkspaceId,
//! #              conn: catalerum_core::id::ConnectionId) -> catalerum_ingest::Result<()> {
//! // In the binary: spawn the worker once, alongside serving.
//! let _worker = catalerum_ingest::spawn_worker(store.clone());
//! // Anywhere a connection should sync: enqueue a durable job.
//! catalerum_ingest::enqueue_sync(&store, ws, conn).await?;
//! # Ok(()) }
//! ```

#![forbid(unsafe_code)]

pub mod automation;
pub mod chunk;
pub mod collect;
pub mod collect_sql;
pub mod curate;
pub mod dedup;
pub mod email;
pub mod embed;
pub mod entity_dedup;
pub mod error;
pub mod google_tokens;
pub mod graph;
pub mod object;
pub mod outlook_tokens;
pub mod profile;
pub mod schedule;
pub mod sync;
pub mod worker;

pub use automation::{
    dispatch_trigger_event, enqueue_run_automation, run_automation, AutomationContext,
    RunAutomationPayload, JOB_KIND_RUN_AUTOMATION,
};
pub use chunk::{chunk_text, ChunkConfig};
pub use collect::{
    enqueue_collect, enqueue_collect_now, run_collect_calendar, run_collect_calendar_with,
    run_collect_email, run_collect_email_with, CollectPayload, CollectReport,
    JOB_KIND_COLLECT_CALENDAR, JOB_KIND_COLLECT_EMAIL,
};
pub use collect_sql::{run_collect_sql, run_collect_sql_with, JOB_KIND_COLLECT_SQL};
pub use curate::{
    enqueue_extract_memories, extract_memories, CurateContext, ExtractMemoriesPayload,
    ExtractReport, JOB_KIND_EXTRACT_MEMORIES,
};
pub use dedup::{
    normalize_memory_text, store_memory_deduped, MemoryDedupIndex, MemoryStoreOutcome,
    MemoryStoreStatus, MEMORY_DEDUP_SIMILARITY_THRESHOLD,
};
pub use email::{
    email_text, enqueue_ingest_email, ingest_email, IngestEmailPayload, JOB_KIND_INGEST_EMAIL,
};
pub use embed::{
    enqueue_ingest_memory, enqueue_ingest_note, ingest_memory, ingest_note, EmbedContext,
    IngestConfig, IngestMemoryPayload, IngestNotePayload, IngestReport, JOB_KIND_INGEST_MEMORY,
    JOB_KIND_INGEST_NOTE,
};
pub use entity_dedup::{
    entity_display_name, entity_id, normalize_entity_name, project_entity_deduped,
    resolve_entities, resolve_entity, EntityStoreOutcome, EntityStoreStatus,
};
pub use error::{IngestError, Result};
pub use google_tokens::{
    gmail_token_store_for, google_token_store_for, reseal_plaintext_gmail_if_applicable,
    SecretGoogleTokenStore,
};
pub use graph::{
    enqueue_project_event, enqueue_project_link, enqueue_project_note, project_event_to_graph,
    project_link_to_graph, project_note_to_graph, GraphContext, ProjectEventPayload,
    ProjectLinkPayload, ProjectNotePayload, ProjectReport, JOB_KIND_PROJECT_EVENT,
    JOB_KIND_PROJECT_LINK, JOB_KIND_PROJECT_NOTE,
};
pub use object::{
    enqueue_ingest_object, extract_text, is_text_like, IngestObjectPayload, ObjectIngestContext,
    ObjectIngestReport, OcrContext, JOB_KIND_INGEST_OBJECT,
};
pub use outlook_tokens::{outlook_token_store_for, SecretOutlookTokenStore};
pub use profile::{
    dispatch_channel_to_profiles, enqueue_run_profile, run_profile_job, RunProfilePayload,
    JOB_KIND_RUN_PROFILE,
};
pub use schedule::{
    scan_calendar_event_triggers, scan_collect_triggers, scan_graph_queries, scan_schedules,
    ScheduleWorker,
};
// Re-export the calendar crate's Google + Microsoft OAuth pieces so the API
// layer (which has no reqwest / no direct catalerum-calendar dep) can drive the
// /auth/google and /auth/microsoft flows.
pub use catalerum_calendar::{
    google_exchange_code, outlook_auth_url, outlook_exchange_code, GoogleCalendarProvider,
    GoogleTokens, GoogleWatchChannel, OutlookTokens, GOOGLE_AUTH_URL, GOOGLE_CALENDAR_EVENTS_SCOPE,
    GOOGLE_CALENDAR_READONLY_SCOPE, OUTLOOK_CALENDAR_SCOPES,
};
// Re-export the provider factory so the API's event write-back seam
// (`calendar_writeback`) can build a live provider without a direct
// catalerum-calendar dependency, exactly like the OAuth pieces above.
pub use catalerum_calendar::provider_from_connection_with;
// Re-export the Gmail read-only scope so the API's /auth/google connect route can
// request it for `kind=email` (the api has no direct catalerum-email dep).
pub use catalerum_email::GMAIL_READONLY_SCOPE;
pub use sync::{event_to_upsert, sync_connection, sync_connection_with, SyncReport};
pub use worker::{
    enqueue_sync, spawn_worker, spawn_worker_with, SyncCalendarPayload, SyncWorker, WorkerConfig,
    JOB_KIND_SYNC_CALENDAR,
};

// ---------------------------------------------------------------------------
// Paged-sync draining (SOUL §8/§10/§28)
// ---------------------------------------------------------------------------

use catalerum_core::model::Cursor;

/// Hard cap on pages drained in a single provider sync run — an anti-infinite-loop
/// bound for a misbehaving provider that reports `has_more` without ever advancing
/// its cursor. Far above any real sync (a paged provider returns hundreds–thousands
/// of items per page, so this bounds millions of items).
pub(crate) const MAX_SYNC_PAGES: usize = 1000;

/// Decide the cursor for the **next** page when draining a paged provider sync
/// ([`SyncBatch::has_more`](catalerum_core::provider::SyncBatch), SOUL §8/§10/§28):
/// returns `Some(next)` to fetch another page, or `None` to stop. Stops when the
/// provider reports no more data, when the cursor did **not** advance (a provider
/// looping the same page), or when [`MAX_SYNC_PAGES`] pages have been drained. Pure
/// — the loop body that *applies* each page lives in the per-item sync fns
/// (`sync_mailbox` / `sync_calendar`), which previously fetched only the first page
/// and silently dropped a paged provider's backlog until the next scheduled run.
pub(crate) fn next_sync_page(
    has_more: bool,
    used_cursor: Option<&Cursor>,
    next_cursor: &Cursor,
    pages_done: usize,
    max_pages: usize,
) -> Option<Cursor> {
    if !has_more || pages_done >= max_pages {
        return None;
    }
    // The cursor must advance, else the next page would refetch the same data forever.
    if used_cursor == Some(next_cursor) {
        return None;
    }
    Some(next_cursor.clone())
}

#[cfg(test)]
mod paging_tests {
    use super::{next_sync_page, MAX_SYNC_PAGES};
    use catalerum_core::model::Cursor;

    #[test]
    fn next_sync_page_drains_until_no_more_or_capped_or_stalled() {
        let c0 = Cursor::new("p0");
        let c1 = Cursor::new("p1");
        // has_more + an advancing cursor + under the cap → fetch the next page.
        assert_eq!(
            next_sync_page(true, Some(&c0), &c1, 1, MAX_SYNC_PAGES),
            Some(c1.clone())
        );
        // No more data → stop regardless of cursor.
        assert_eq!(
            next_sync_page(false, Some(&c0), &c1, 1, MAX_SYNC_PAGES),
            None
        );
        // Cursor did not advance (next == just-used) → stop (no infinite loop).
        assert_eq!(
            next_sync_page(true, Some(&c1), &c1, 1, MAX_SYNC_PAGES),
            None
        );
        // Page cap reached → stop even though more is claimed.
        assert_eq!(next_sync_page(true, Some(&c0), &c1, 5, 5), None);
        // The first page from a fresh (None) cursor advances normally.
        assert_eq!(next_sync_page(true, None, &c0, 1, MAX_SYNC_PAGES), Some(c0));
    }
}
