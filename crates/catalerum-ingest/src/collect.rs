//! Email/calendar **collect** jobs (SOUL §10/§11/§28) — the durable head of a
//! user-authored ingest graph.
//!
//! A `CollectEmail` / `CollectCalendar` *trigger* (filled with a connection) is a
//! poll source: the [`ScheduleWorker`](crate::schedule::ScheduleWorker) enqueues a
//! lightweight [`JOB_KIND_COLLECT_EMAIL`] / [`JOB_KIND_COLLECT_CALENDAR`] job on the
//! trigger's `every` cadence (keeping heavy provider I/O off the 60s scheduler
//! clock), and a worker holding an [`AutomationContext`] claims it and runs
//! [`run_collect_email`] / [`run_collect_calendar`]. That pulls new external items
//! from the provider and **fires one automation run per genuinely-new item**, with
//! the item carried on the run's trigger for a downstream `WriteEmail`/`WriteEvent`
//! action to persist. Nothing is stored or searchable unless the graph writes it
//! (§28); adding a connection provisions nothing — an unwired connection is dormant.
//!
//! ## Cursor commit (`commit_on`, SOUL §11/§29)
//! The trigger may carry a `commit_on` node id — a downstream write node. The
//! connection's per-source cursor advances over an item **only after that node
//! `Succeeded`** for the item's run (a `Condition`-`Skipped` write counts as
//! intentionally committed). With `commit_on` unset the cursor advances regardless
//! (fire-and-forget: the run still happens, only durable storage of the item is the
//! author's responsibility).
//!
//! ## Execution model (the §29 out-of-order resolution)
//! Items are processed **in order, inline** within the single collect job: each new
//! item gets its own real [`AutomationRun`] (full §11 audit), and the
//! contiguous-committed-prefix is maintained trivially by in-order processing. This
//! sidesteps the unsolved out-of-order ledger + sync-token compare-and-set race a
//! fan-out-to-separate-jobs model would create. The collect job is single-claimed
//! (job-queue `FOR UPDATE SKIP LOCKED`) and its enqueue is single-fired per due
//! window by the scheduler's distributed lock, so only one pod runs it at a time.
//!
//! ## Idempotency / at-least-once
//! A crashed/redelivered collect job re-pulls from the un-advanced cursor and skips
//! items already in the per-source **committed ledger**, which is persisted after
//! **each item** (so a committed item's `LlmAgent`/`WriteEmail` never re-runs — no
//! double-spend). The remaining at-least-once window is exactly the item **in
//! flight** when the crash hit (its run started but the ledger hadn't been written):
//! that one item — and the uncommitted tail — re-run. `WriteEmail`/`WriteEvent` upsert
//! idempotently by `(mailbox_id, uid)` / `(calendar_id, uid)`, so a re-run never
//! duplicates stored items.
//!
//! The §29 "idempotent redelivery" hole — a re-run of that in-flight item
//! double-spending a non-idempotent `LlmAgent` / re-running a `LabelEmail` — is now
//! closed at the **engine** level (see `catalerum_automation::ActionKind::is_idempotent`
//! and the DAG executor's redelivery gate): on the re-run, `WriteEmail` finds the
//! message already stored and reports `newly_written: false`, which latches the run as
//! a **redelivery**; the executor then auto-**Skips** the downstream non-idempotent
//! nodes while still running the idempotent write so `commit_on` finally advances the
//! cursor. So the re-run stores the item (again, harmlessly) and commits, but does
//! **not** re-spend tokens or re-label. Calendar's `WriteEvent` emits the same signal
//! (via `EventRepo::get_by_uid`), so a `CollectCalendar → WriteEvent → LlmAgent` flow is
//! redelivery-gated exactly like the email flow.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use catalerum_automation::{calendar_event_filter_matches, execute_for_job, Graph, Trigger};
use catalerum_core::capability::{
    allows as capability_allows, Action as CapAction, Capability, Resource,
};
use catalerum_core::id::{AutomationId, CalendarId, ConnectionId, MailboxId, WorkspaceId};
use catalerum_core::model::{
    AutomationRun, Calendar, Connection, Cursor, Email, Event, Mailbox, RunStatus, StepStatus,
};
use catalerum_core::provider::{CalendarProvider, EmailProvider, SyncBatch};
use catalerum_store::{SecretStore, Store};

use crate::automation::AutomationContext;
use crate::error::{IngestError, Result};

/// The `job_queue.kind` token for an email-collect job (SOUL §10/§28). Enqueued by
/// the scheduler on a `CollectEmail` trigger's cadence; a worker with an
/// [`AutomationContext`] runs [`run_collect_email`].
pub const JOB_KIND_COLLECT_EMAIL: &str = "collect_email";
/// The `job_queue.kind` token for a calendar-collect job (SOUL §8/§10).
pub const JOB_KIND_COLLECT_CALENDAR: &str = "collect_calendar";

/// How many new items a **first** poll of a source processes (the rest drain across
/// later polls — the §29 first-run-backfill-flood guard, so a large existing
/// mailbox doesn't run one automation per historical message at once).
const FIRST_POLL_CAP: usize = 200;
/// How many new items a steady-state poll processes (a generous ceiling; a healthy
/// source yields a handful per cadence).
const STEADY_POLL_CAP: usize = 1000;

/// The JSON payload of a collect job (SOUL §10/§28): which automation to run and the
/// collect trigger spec to pull for. `workspace_id` is optional on the wire (the
/// worker falls back to the job row), matching the other job kinds' contract.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CollectPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    /// The automation whose collect trigger fired (and whose graph runs per item).
    pub automation_id: AutomationId,
    /// The `CollectEmail`/`CollectCalendar` trigger spec (carries the connection,
    /// mailbox?/calendar?, filter?, commit_on?, backfill_window?).
    pub trigger: Value,
}

impl CollectPayload {
    /// Build a payload carrying an explicit workspace scope.
    #[must_use]
    pub fn new(workspace_id: WorkspaceId, automation_id: AutomationId, trigger: Value) -> Self {
        Self {
            workspace_id: Some(workspace_id),
            automation_id,
            trigger,
        }
    }
}

/// Enqueue a durable collect job for `automation_id`'s collect `trigger` (SOUL §10).
/// The job kind is chosen from the trigger variant. Returns the enqueued job id.
///
/// # Errors
/// If the trigger is not a collect trigger, or the enqueue fails.
pub async fn enqueue_collect(
    store: &Store,
    workspace_id: WorkspaceId,
    automation_id: AutomationId,
    trigger: &Trigger,
) -> Result<uuid::Uuid> {
    let kind = match trigger {
        Trigger::CollectEmail { .. } => JOB_KIND_COLLECT_EMAIL,
        Trigger::CollectCalendar { .. } => JOB_KIND_COLLECT_CALENDAR,
        Trigger::CollectSql { .. } => crate::collect_sql::JOB_KIND_COLLECT_SQL,
        _ => {
            return Err(IngestError::invalid_job(
                "enqueue_collect called with a non-collect trigger".to_string(),
            ))
        }
    };
    let payload = CollectPayload::new(
        workspace_id,
        automation_id,
        serde_json::to_value(trigger).map_err(|e| IngestError::invalid_job(e.to_string()))?,
    );
    let job = store
        .job_queue()
        .enqueue(
            Some(workspace_id),
            kind,
            serde_json::to_value(payload).map_err(|e| IngestError::invalid_job(e.to_string()))?,
            None,
        )
        .await?;
    Ok(job.id)
}

/// Enqueue **one immediate collect poll** for `automation` — the "collect now"
/// primitive (SOUL §29). This resolves the §29 question "is a one-shot 'collect now'
/// just a manual run of the automation?": a collect automation's "run" is a *poll*
/// that fans out one `AutomationRun` per new item, **not** a bare `run_automation` of
/// its actions (a `WriteEmail` with no trigger item is meaningless). So a manual
/// "collect now" enqueues the very same [`enqueue_collect`] job the scheduler would on
/// the trigger's cadence — only right now, bypassing the [`due_bucket`] cadence gate.
///
/// Returns `Some(job_id)` for the first `CollectEmail`/`CollectCalendar` trigger on the
/// automation, or `None` if it has no (parseable) collect trigger — a non-collect
/// automation is not "collectable", so the caller can surface that (e.g. a `400`).
///
/// [`due_bucket`]: crate::schedule
///
/// # Errors
/// If the enqueue fails.
pub async fn enqueue_collect_now(
    store: &Store,
    workspace_id: WorkspaceId,
    automation: &catalerum_core::Automation,
) -> Result<Option<uuid::Uuid>> {
    let Some(trigger) = automation.triggers.iter().find_map(|t| {
        let trigger = serde_json::from_value::<Trigger>(t.clone()).ok()?;
        trigger.is_collect().then_some(trigger)
    }) else {
        return Ok(None);
    };
    let id = enqueue_collect(store, workspace_id, automation.id, &trigger).await?;
    Ok(Some(id))
}

/// What a single collect run did, for logging / metrics / tests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CollectReport {
    /// New external items that were fired as automation runs this poll.
    pub runs_fired: usize,
    /// Items whose `commit_on` write succeeded (so the cursor advanced over them).
    pub committed: usize,
    /// Sources (mailboxes/calendars) polled.
    pub sources: usize,
    /// Upstream-deleted items reconciled this poll — the local row + its derived
    /// projection hard-deleted (SOUL §11/§28). Provider-surfaced (IMAP/JMAP/Gmail/
    /// CalDAV) or snapshot-diffed (Maildir/local `.ics`/webcal).
    pub deleted: usize,
}

// ---------------------------------------------------------------------------
// Per-source committed-prefix ledger (packed into connection.sync_token)
// ---------------------------------------------------------------------------

/// The collect cursor ledger persisted in a connection's single `sync_token`
/// column (SOUL §11/§29): per source (mailbox/calendar `external_id`), the provider
/// cursor we've fully committed up to, plus the set of committed item uids not yet
/// behind that cursor (the dedup set that prevents re-running an already-committed
/// item when the cursor hasn't advanced or a snapshot provider re-emits it).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CollectLedger {
    #[serde(default)]
    pub(crate) sources: BTreeMap<String, SourceState>,
}

/// Per-source collect state (see [`CollectLedger`]).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SourceState {
    /// The provider cursor we've fully committed up to. `None` = never polled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cursor: Option<String>,
    /// Committed item uids not yet behind the cursor (dedup set).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub(crate) committed: BTreeSet<String>,
    /// Whether the first poll (backfill bound) has happened.
    #[serde(default)]
    pub(crate) initialized: bool,
}

impl CollectLedger {
    /// Decode the ledger from a connection's `sync_token`. A token that isn't a
    /// collect ledger (e.g. an old whole-connection `CursorMap` from the on-demand
    /// calendar sync path) decodes to an empty ledger — collect then re-discovers
    /// items (idempotent upserts make that safe), rather than mis-reading a foreign
    /// shape.
    pub(crate) fn decode(cursor: Option<&Cursor>) -> Self {
        cursor
            .and_then(|c| serde_json::from_str::<CollectLedger>(&c.0).ok())
            .unwrap_or_default()
    }

    /// Encode the ledger back to a [`Cursor`] for `update_cursor`.
    pub(crate) fn encode(&self) -> Cursor {
        Cursor::new(serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string()))
    }
}

// ---------------------------------------------------------------------------
// Email collect
// ---------------------------------------------------------------------------

/// Run an email collect job (SOUL §10/§28): build the provider from the connection,
/// poll each target mailbox, and fire one automation run per genuinely-new message,
/// advancing the cursor over the contiguous committed prefix.
///
/// # Errors
/// A provider/store failure (a retryable job failure — the scheduler re-enqueues
/// next window). A disabled/deleted automation or an unconfigured connection is a
/// no-op (`Ok` with an empty report).
pub async fn run_collect_email(
    store: &Store,
    ctx: &AutomationContext,
    workspace_id: WorkspaceId,
    payload: &CollectPayload,
) -> Result<CollectReport> {
    run_collect_email_with(store, ctx, workspace_id, payload, None).await
}

/// Like [`run_collect_email`], but threads the encrypted secret store so a sealed
/// **Gmail** connection can build its OAuth-backed provider (SOUL §13/§28), reading
/// the same unified Google token store the calendar path uses. A legacy plaintext
/// Gmail connection (no `credential_ref`) ignores the seam and keeps syncing.
pub async fn run_collect_email_with(
    store: &Store,
    ctx: &AutomationContext,
    workspace_id: WorkspaceId,
    payload: &CollectPayload,
    secrets: Option<&Arc<SecretStore>>,
) -> Result<CollectReport> {
    let trigger: Trigger = serde_json::from_value(payload.trigger.clone())
        .map_err(|e| IngestError::invalid_job(format!("collect_email bad trigger: {e}")))?;
    let Trigger::CollectEmail {
        connection,
        mailbox,
        filter,
        commit_on,
        backfill_window,
        ..
    } = trigger
    else {
        return Err(IngestError::invalid_job(
            "collect_email job carries a non-CollectEmail trigger".to_string(),
        ));
    };
    let connection_id = parse_connection(&connection)?;

    let Some(automation) = load_enabled(store, workspace_id, payload.automation_id).await? else {
        return Ok(CollectReport::default());
    };

    // Enforce the collect capability (SOUL §11/§19) under the automation's recorded
    // grant, before any provider I/O — fails the run closed if the grant omits the
    // connection this trigger pulls from.
    authorize_collect(store, workspace_id, &automation, "email", &connection).await?;

    let row = store
        .connections()
        .get_row(workspace_id, connection_id)
        .await?;
    if catalerum_email::is_unconfigured(row.config()) {
        tracing::debug!(workspace = %workspace_id, connection = %connection_id,
            "skipping unconfigured email connection (nothing to collect)");
        return Ok(CollectReport::default());
    }
    let connection_dom: Connection = row.clone().try_into().map_err(IngestError::Store)?;

    // Opportunistic resealing (SOUL §13/§28): before syncing, migrate a legacy
    // plaintext Gmail connection onto the encrypted store (or heal a half-resealed
    // one) when a master key is configured. Best-effort — a failure leaves the
    // connection intact and this run still syncs on whatever provider it built below.
    // A reseal here mutates the DB row, so THIS run's sync uses the pre-reseal
    // (plaintext) provider (in-memory, still valid); the next run reads the sealed row.
    crate::google_tokens::reseal_plaintext_gmail_if_applicable(
        &store.connections(),
        secrets,
        &connection_dom,
        row.config(),
    )
    .await;

    let gmail_tokens =
        crate::google_tokens::gmail_token_store_for(secrets, &connection_dom, row.config());
    let provider = catalerum_email::provider_from_connection_with(
        &connection_dom,
        row.config(),
        gmail_tokens,
    )?;
    let incremental = provider.is_incremental();

    let mut ledger = CollectLedger::decode(connection_dom.cursor.as_ref());
    let mut report = CollectReport::default();
    let now = Utc::now();

    let mailboxes = select_email_sources(&provider, mailbox.as_deref()).await?;
    report.sources = mailboxes.len();
    for mb in mailboxes {
        // Ensure the store mailbox row exists so a downstream WriteEmail upsert has a
        // valid FK and the email's mailbox_id (a stable v5 id over (connection,
        // external_id)) already matches it.
        let stored_mb = store
            .mailboxes()
            .upsert(
                workspace_id,
                connection_id,
                &mb.external_id,
                &mb.name,
                mb.read_only,
            )
            .await?;

        let mut state = ledger
            .sources
            .get(&mb.external_id)
            .cloned()
            .unwrap_or_default();
        let prior_cursor = state.cursor.clone().map(Cursor::new);
        let (batch_upserts, deletions, next_cursor, drained_fully) =
            drain_email(&provider, &stored_mb, prior_cursor.clone()).await?;

        // Upstream deletions (SOUL §11/§28): the provider names uids gone at the
        // source (IMAP's vanished-uid delta, JMAP's per-mailbox snapshot diff,
        // Gmail's `messageDeleted` history). Reconcile them **inline** — there is
        // no deletion action node to route a per-item run through, and the store
        // delete + derived purge is idempotent (a redelivered deletion, or one for
        // a uid a fire-and-forget/filtered graph never wrote, is a no-op) — and
        // drop each uid from the committed ledger so the entry doesn't linger (and
        // a re-appearing uid re-collects). A failed delete fails the job (retried;
        // the un-advanced cursor re-emits the deletion), so deletions advance the
        // cursor exactly like writes: only once they've been applied.
        for uid in &deletions {
            state.committed.remove(uid);
            if delete_stored_email(store, ctx, workspace_id, stored_mb.id, uid).await? {
                report.deleted += 1;
            }
        }

        let cutoff = backfill_cutoff(backfill_window.as_ref(), state.initialized, now);
        let cap = if state.initialized {
            STEADY_POLL_CAP
        } else {
            FIRST_POLL_CAP
        };
        let cursor_changed = state.cursor.as_deref() != Some(next_cursor.0.as_str());
        let batch_uids: BTreeSet<String> = batch_upserts.iter().map(|e| e.uid.clone()).collect();

        // Snapshot-provider deletion reconcile (Maildir, SOUL §11/§28): a snapshot
        // backend re-emits the full uid set each changed poll and never emits
        // `deletions`, so a stored email absent from a complete snapshot was
        // deleted at the source. Guarded like the on-demand sync path's reconcile:
        // only on a fully-drained, actually-changed (cursor moved), non-empty
        // snapshot — a no-change poll (empty re-list) or an emptied/partial read
        // never masquerades as "delete everything".
        if !incremental && drained_fully && cursor_changed && !batch_uids.is_empty() {
            report.deleted +=
                reconcile_email_snapshot(store, ctx, workspace_id, stored_mb.id, &batch_uids)
                    .await?;
        }

        // Partition the batch: items to run vs. items to mark seen-but-skip. A
        // pre-cutoff message on the FIRST poll is marked committed (seen) — not run,
        // not forgotten — so once the cutoff lifts in steady state it can't re-enter
        // and flood the history (the first-poll backfill bound, §11/§29). A
        // filter-excluded message is just skipped, never marked committed. A SNAPSHOT
        // provider re-emits the whole source each poll, so it's re-evaluated cheaply
        // and widening the filter later picks it up; an INCREMENTAL provider's cursor
        // advances past it, so a later filter-widen can't re-surface it (that would
        // need a deliberate from-scratch re-scan).
        let mut new_items: Vec<Email> = Vec::new();
        for e in batch_upserts {
            if state.committed.contains(&e.uid) {
                continue;
            }
            if !email_filter_matches(filter.as_ref(), &e) {
                continue;
            }
            if !passes_cutoff(e.received_at, cutoff) {
                state.committed.insert(e.uid);
                continue;
            }
            new_items.push(e);
        }
        new_items.sort_by_key(|e| sort_key(e.received_at, &e.uid));

        let total_new = new_items.len();
        // The cursor advances only if the whole drained batch was processed (no
        // over-cap tail) and every processed item committed.
        let mut all_committed = drained_fully && total_new <= cap;
        for item in new_items.into_iter().take(cap) {
            let mut item = item;
            item.mailbox_id = stored_mb.id;
            // Lift the transient raw bytes out **before** serializing the item onto the
            // trigger (raw is `#[serde(skip)]`, so it never rides the trigger/run audit
            // anyway) and MIME-extract the attachment parts here, where a parser lives
            // (SOUL §9/§28/§29). Both are archived to object storage after a successful
            // write — see the `ctx.runner().archive_email` call below.
            let raw = item.raw.take();
            let attachments = raw
                .as_deref()
                .map(catalerum_email::extract_attachments)
                .unwrap_or_default();
            let trigger_json = json!({
                "kind": JOB_KIND_COLLECT_EMAIL,
                "connection": connection,
                "mailbox": mb.external_id,
                "item": serde_json::to_value(&item)
                    .map_err(|e| IngestError::invalid_job(e.to_string()))?,
            });
            report.runs_fired += 1;
            let committed = run_item(
                store,
                ctx,
                workspace_id,
                &automation,
                trigger_json,
                commit_on.as_deref(),
            )
            .await?;
            if committed {
                state.committed.insert(item.uid.clone());
                report.committed += 1;
                // Archive raw `.eml` + attachments as objects and link them onto the
                // stored row (SOUL §9/§28/§29). Best-effort + idempotent: the runner
                // skips a redelivery whose row is already archived, and skips cleanly
                // when no files store is configured — the write already committed, so
                // archival never blocks the cursor. JMAP items carry no raw bytes yet,
                // so this is a no-op there (deferred: JMAP blob download).
                ctx.runner()
                    .archive_email(workspace_id, stored_mb.id, &item.uid, raw, attachments)
                    .await;
            } else {
                all_committed = false;
            }
            // Persist after each item so a crash/redelivery re-runs at most the
            // current item, not the whole source (§29 at-least-once window).
            persist_source(
                store,
                workspace_id,
                connection_id,
                &mut ledger,
                &mb.external_id,
                &state,
            )
            .await?;
        }

        advance_source(
            &mut state,
            all_committed,
            &next_cursor,
            cursor_changed,
            incremental,
            &batch_uids,
        );
        persist_source(
            store,
            workspace_id,
            connection_id,
            &mut ledger,
            &mb.external_id,
            &state,
        )
        .await?;
    }

    Ok(report)
}

/// The mailbox(es) a `CollectEmail` polls: the one whose `external_id`/`name`
/// matches the trigger's optional `mailbox`, else every mailbox the provider
/// exposes (today each email backend exposes exactly one — its configured folder).
async fn select_email_sources(
    provider: &Arc<dyn EmailProvider>,
    mailbox: Option<&str>,
) -> Result<Vec<Mailbox>> {
    let all = provider.list_mailboxes().await?;
    Ok(match mailbox {
        None => all,
        Some(want) => all
            .into_iter()
            .filter(|m| {
                m.external_id.eq_ignore_ascii_case(want) || m.name.eq_ignore_ascii_case(want)
            })
            .collect(),
    })
}

/// Drain every page of an email provider sync from `cursor`, accumulating all
/// upserts + deletions and returning the final cursor and whether the drain
/// completed (vs. stopped on the page cap / a stalled cursor).
async fn drain_email(
    provider: &Arc<dyn EmailProvider>,
    mailbox: &Mailbox,
    cursor: Option<Cursor>,
) -> Result<(Vec<Email>, Vec<String>, Cursor, bool)> {
    let mut upserts = Vec::new();
    let mut deletions = Vec::new();
    let mut page_cursor = cursor;
    let mut pages = 0usize;
    loop {
        let batch: SyncBatch<Email> = provider.sync(mailbox, page_cursor.clone()).await?;
        pages += 1;
        upserts.extend(batch.upserts);
        deletions.extend(batch.deletions);
        let next = batch.next_cursor;
        match crate::next_sync_page(
            batch.has_more,
            page_cursor.as_ref(),
            &next,
            pages,
            crate::MAX_SYNC_PAGES,
        ) {
            Some(c) => page_cursor = Some(c),
            None => return Ok((upserts, deletions, next, !batch.has_more)),
        }
    }
}

// ---------------------------------------------------------------------------
// Calendar collect
// ---------------------------------------------------------------------------

/// Run a calendar collect job (SOUL §8/§10): the calendar twin of
/// [`run_collect_email`] — poll each target calendar and fire one run per new event
/// for a downstream `WriteEvent`.
///
/// # Errors
/// As [`run_collect_email`].
///
/// A Google calendar connection needs the OAuth token seam; use
/// [`run_collect_calendar_with`] and pass the secret store. This entry (no
/// secrets) works for local/CalDAV/webcal and errors clearly on a Google source.
pub async fn run_collect_calendar(
    store: &Store,
    ctx: &AutomationContext,
    workspace_id: WorkspaceId,
    payload: &CollectPayload,
) -> Result<CollectReport> {
    run_collect_calendar_with(store, ctx, workspace_id, payload, None).await
}

/// Like [`run_collect_calendar`], but threads the encrypted secret store so a
/// Google calendar connection can build its OAuth-backed provider (SOUL §13/§16 M7).
pub async fn run_collect_calendar_with(
    store: &Store,
    ctx: &AutomationContext,
    workspace_id: WorkspaceId,
    payload: &CollectPayload,
    secrets: Option<&Arc<SecretStore>>,
) -> Result<CollectReport> {
    let trigger: Trigger = serde_json::from_value(payload.trigger.clone())
        .map_err(|e| IngestError::invalid_job(format!("collect_calendar bad trigger: {e}")))?;
    let Trigger::CollectCalendar {
        connection,
        calendar,
        filter,
        commit_on,
        backfill_window,
        ..
    } = trigger
    else {
        return Err(IngestError::invalid_job(
            "collect_calendar job carries a non-CollectCalendar trigger".to_string(),
        ));
    };
    let connection_id = parse_connection(&connection)?;

    let Some(automation) = load_enabled(store, workspace_id, payload.automation_id).await? else {
        return Ok(CollectReport::default());
    };

    // Enforce the collect capability (SOUL §11/§19) under the automation's recorded
    // grant, before any provider I/O — fails the run closed if the grant omits the
    // connection this trigger pulls from.
    authorize_collect(store, workspace_id, &automation, "calendar", &connection).await?;

    let row = store
        .connections()
        .get_row(workspace_id, connection_id)
        .await?;
    let connection_dom: Connection = row.clone().try_into().map_err(IngestError::Store)?;
    let google_tokens =
        crate::google_tokens::google_token_store_for(secrets, &connection_dom, row.config());
    let outlook_tokens =
        crate::outlook_tokens::outlook_token_store_for(secrets, &connection_dom, row.config());
    let provider = catalerum_calendar::provider_from_connection_with(
        &connection_dom,
        row.config(),
        google_tokens,
        outlook_tokens,
    )?;
    let incremental = provider.is_incremental();

    let mut ledger = CollectLedger::decode(connection_dom.cursor.as_ref());
    let mut report = CollectReport::default();
    let now = Utc::now();

    // Provider calendars the user deleted locally (`DELETE /calendars/{id}`) are
    // excluded from re-`upsert` so the deletion sticks across polls (SOUL §8/§11).
    let excluded: BTreeSet<String> = store
        .calendars()
        .excluded_external_ids(workspace_id, connection_id)
        .await?
        .into_iter()
        .collect();
    let calendars = select_calendar_sources(&provider, calendar.as_deref(), &excluded).await?;
    report.sources = calendars.len();
    for cal in calendars {
        let stored_cal = store
            .calendars()
            .upsert(
                workspace_id,
                connection_id,
                &cal.external_id,
                &cal.name,
                cal.read_only,
            )
            .await?;

        let mut state = ledger
            .sources
            .get(&cal.external_id)
            .cloned()
            .unwrap_or_default();
        let prior_cursor = state.cursor.clone().map(Cursor::new);
        let (batch_upserts, deletions, next_cursor, drained_fully) =
            drain_calendar(&provider, &stored_cal, prior_cursor.clone()).await?;

        // Upstream deletions (SOUL §8/§11): CalDAV's sync-collection REPORT names
        // deleted hrefs, parsed to event uids. Reconciled inline like the email
        // path — hard-delete the local row + purge its `:Event` graph node —
        // idempotently (a redelivered or never-written uid is a no-op), with the
        // uid dropped from the committed ledger. A failed delete fails the job, so
        // deletions advance the cursor exactly like writes.
        for uid in &deletions {
            state.committed.remove(uid);
            if delete_stored_event(store, workspace_id, stored_cal.id, uid).await? {
                report.deleted += 1;
            }
        }

        let cutoff = backfill_cutoff(backfill_window.as_ref(), state.initialized, now);
        let cap = if state.initialized {
            STEADY_POLL_CAP
        } else {
            FIRST_POLL_CAP
        };
        let cursor_changed = state.cursor.as_deref() != Some(next_cursor.0.as_str());
        let batch_uids: BTreeSet<String> = batch_upserts.iter().map(|e| e.uid.clone()).collect();

        // Snapshot-provider deletion reconcile (local `.ics` / webcal, SOUL
        // §8/§11): a snapshot backend re-emits the full event set each changed
        // poll and never emits `deletions`; a stored event absent from a complete
        // snapshot was deleted at the source. Same guards as the email twin.
        if !incremental && drained_fully && cursor_changed && !batch_uids.is_empty() {
            report.deleted +=
                reconcile_event_snapshot(store, workspace_id, stored_cal.id, &batch_uids).await?;
        }

        let mut new_items: Vec<Event> = Vec::new();
        for e in batch_upserts {
            if state.committed.contains(&e.uid) {
                continue;
            }
            if !calendar_event_filter_matches(
                filter.as_ref(),
                &e.summary,
                e.location.as_deref(),
                e.body.as_deref(),
            ) {
                continue;
            }
            if !passes_cutoff(Some(e.start), cutoff) {
                state.committed.insert(e.uid);
                continue;
            }
            new_items.push(e);
        }
        new_items.sort_by_key(|e| sort_key(Some(e.start), &e.uid));

        let total_new = new_items.len();
        let mut all_committed = drained_fully && total_new <= cap;
        for item in new_items.into_iter().take(cap) {
            let mut item = item;
            item.calendar_id = stored_cal.id;
            let trigger_json = json!({
                "kind": JOB_KIND_COLLECT_CALENDAR,
                "connection": connection,
                "calendar": cal.external_id,
                "item": serde_json::to_value(&item)
                    .map_err(|e| IngestError::invalid_job(e.to_string()))?,
            });
            report.runs_fired += 1;
            let committed = run_item(
                store,
                ctx,
                workspace_id,
                &automation,
                trigger_json,
                commit_on.as_deref(),
            )
            .await?;
            if committed {
                state.committed.insert(item.uid.clone());
                report.committed += 1;
            } else {
                all_committed = false;
            }
            persist_source(
                store,
                workspace_id,
                connection_id,
                &mut ledger,
                &cal.external_id,
                &state,
            )
            .await?;
        }

        advance_source(
            &mut state,
            all_committed,
            &next_cursor,
            cursor_changed,
            incremental,
            &batch_uids,
        );
        persist_source(
            store,
            workspace_id,
            connection_id,
            &mut ledger,
            &cal.external_id,
            &state,
        )
        .await?;
    }

    Ok(report)
}

/// The calendar(s) a `CollectCalendar` polls: the one matching the trigger's
/// optional `calendar`, else every calendar the connection exposes — minus any
/// the user deleted locally (`excluded`, by `external_id`), which are never
/// re-ingested even when named explicitly by the trigger.
async fn select_calendar_sources(
    provider: &Arc<dyn CalendarProvider>,
    calendar: Option<&str>,
    excluded: &BTreeSet<String>,
) -> Result<Vec<Calendar>> {
    let all = provider
        .list_calendars()
        .await?
        .into_iter()
        .filter(|c| !excluded.contains(&c.external_id));
    Ok(match calendar {
        None => all.collect(),
        Some(want) => all
            .filter(|c| {
                c.external_id.eq_ignore_ascii_case(want) || c.name.eq_ignore_ascii_case(want)
            })
            .collect(),
    })
}

/// Drain every page of a calendar provider sync (the calendar twin of
/// [`drain_email`]).
async fn drain_calendar(
    provider: &Arc<dyn CalendarProvider>,
    calendar: &Calendar,
    cursor: Option<Cursor>,
) -> Result<(Vec<Event>, Vec<String>, Cursor, bool)> {
    let mut upserts = Vec::new();
    let mut deletions = Vec::new();
    let mut page_cursor = cursor;
    let mut pages = 0usize;
    loop {
        let batch: SyncBatch<Event> = provider.sync(calendar, page_cursor.clone()).await?;
        pages += 1;
        upserts.extend(batch.upserts);
        deletions.extend(batch.deletions);
        let next = batch.next_cursor;
        match crate::next_sync_page(
            batch.has_more,
            page_cursor.as_ref(),
            &next,
            pages,
            crate::MAX_SYNC_PAGES,
        ) {
            Some(c) => page_cursor = Some(c),
            None => return Ok((upserts, deletions, next, !batch.has_more)),
        }
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Persist a source's collect state into the connection's `sync_token` ledger now
/// (SOUL §11/§29) — called after each item and after each source, so a per-source
/// error or a mid-poll crash keeps the progress already made (committed items are
/// not re-run) instead of replaying the whole source. `update_cursor` rewrites the
/// whole token; the ledger carries every source's state, so an incremental write
/// for one source preserves the others.
pub(crate) async fn persist_source(
    store: &Store,
    workspace_id: WorkspaceId,
    connection_id: ConnectionId,
    ledger: &mut CollectLedger,
    external_id: &str,
    state: &SourceState,
) -> Result<()> {
    ledger
        .sources
        .insert(external_id.to_string(), state.clone());
    store
        .connections()
        .update_cursor(workspace_id, connection_id, Some(&ledger.encode()))
        .await?;
    Ok(())
}

/// Parse a trigger's `connection` string into a [`ConnectionId`].
pub(crate) fn parse_connection(s: &str) -> Result<ConnectionId> {
    s.trim().parse::<ConnectionId>().map_err(|_| {
        IngestError::invalid_job(format!(
            "collect trigger has an invalid connection id `{s}`"
        ))
    })
}

/// Load the automation, returning `None` if it was deleted or disabled after the
/// collect job was enqueued (so a pause/delete settles the job, not a stuck retry).
pub(crate) async fn load_enabled(
    store: &Store,
    workspace_id: WorkspaceId,
    automation_id: AutomationId,
) -> Result<Option<catalerum_core::Automation>> {
    match store.automations().get(workspace_id, automation_id).await {
        Ok(a) if a.enabled => Ok(Some(a)),
        Ok(_) => Ok(None),
        Err(catalerum_store::StoreError::NotFound) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Enforce the **collect capability** for a poll (SOUL §11/§19): a Collect-headed
/// automation may pull from its trigger's connection only if its recorded §19
/// grant covers `email:read@<connection>` / `calendar:read@<connection>` — the
/// grant-scoped form of §11's "the collect grant is scoped to pull that one
/// connection". The pull itself is a provider **read** (nothing lands locally
/// without a downstream `WriteEmail`/`WriteEvent`, each gated `*:write`), so the
/// existing `read` verb expresses it; a dedicated `collect` `Action` variant would
/// today break exhaustive matches on surfaces outside this slice (recorded as a
/// follow-up), and `read` keeps the same deny shape: a grant holding the domain
/// (`email:read`) or the exact connection selector covers it, one holding a
/// *different* connection's selector does not.
///
/// With **no** recorded grant the poll is allowed through: the run then executes
/// under the runner's default bounded authority — workspace-owner identity with
/// base-**Member** capabilities (the §19 `role_grant` fallback) — which holds
/// domain-wide `email:read`/`calendar:read`, so collect is implied for a Member
/// exactly like `write` is (and per-action gates still bind every node). A grant
/// that exists but omits the connection **fails the run closed**
/// ([`IngestError::Forbidden`], a clear job failure — never a silent skip); a
/// dangling `grant_id` fails closed too.
async fn authorize_collect(
    store: &Store,
    workspace_id: WorkspaceId,
    automation: &catalerum_core::Automation,
    domain: &str,
    connection: &str,
) -> Result<()> {
    let Some(grant_id) = automation.grant_id else {
        return Ok(());
    };
    let grant = match store.grants().get(workspace_id, grant_id).await {
        Ok(g) => g,
        Err(catalerum_store::StoreError::NotFound) => {
            return Err(IngestError::forbidden(format!(
            "collect denied: automation `{}` references grant {grant_id}, which no longer exists",
            automation.name
        )))
        }
        Err(e) => return Err(e.into()),
    };
    let requested = Capability::new(CapAction::Read, Resource::new(domain, connection.trim()));
    if capability_allows(&grant, &requested) {
        Ok(())
    } else {
        Err(IngestError::forbidden(format!(
            "collect denied: automation `{}`'s grant `{}` does not cover {domain}:read@{} \
             (the connection its collect trigger pulls from)",
            automation.name,
            grant.name,
            connection.trim(),
        )))
    }
}

/// Hard-delete a provider-deleted email's local row and enqueue the purge of its
/// derived projection — the reconcile-based `ingest_email` job finds the row gone
/// and tears down its document + vector chunks (SOUL §10, the same purge path a
/// deleted note/object uses). Idempotent: a uid never written locally (a
/// fire-and-forget or filter-excluded graph) or already deleted is a no-op
/// (`false`).
async fn delete_stored_email(
    store: &Store,
    ctx: &AutomationContext,
    workspace_id: WorkspaceId,
    mailbox_id: MailboxId,
    uid: &str,
) -> Result<bool> {
    // Read the row **before** deleting so we can reconcile its archived objects
    // (raw `.eml` + attachments, SOUL §9/§28/§29) — `delete_by_uid` returns only the
    // id. A NotFound row is a no-op (never written locally, or already deleted).
    let existing = match store
        .emails()
        .get_by_uid(workspace_id, mailbox_id, uid)
        .await
    {
        Ok(e) => e,
        Err(catalerum_store::StoreError::NotFound) => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let Some(email_id) = store
        .emails()
        .delete_by_uid(workspace_id, mailbox_id, uid)
        .await?
    else {
        return Ok(false);
    };
    // Enqueue-based derived cleanup, like the write path: a failed enqueue is
    // logged, not fatal — the row is already gone from Postgres truth, and the
    // projection is derived/rebuildable (§6.3/§6.4).
    if let Err(e) = crate::email::enqueue_ingest_email(store, workspace_id, email_id).await {
        tracing::warn!(error = %e, email = %email_id,
            "collect: failed to enqueue deletion purge (ingest_email)");
    }
    // Delete the archived objects (raw `.eml` + attachments) so they don't outlive
    // the message they belong to (SOUL §9/§28) — the storage-backed runner deletes
    // each blob and de-indexes it, mirroring the storage route's object-delete
    // cleanup. Best-effort + idempotent; a runner with no store no-ops.
    let keys = archived_object_keys(&existing);
    if !keys.is_empty() {
        ctx.runner().cleanup_email_archive(workspace_id, keys).await;
    }
    tracing::debug!(%email_id, uid, "collect: reconciled upstream email deletion");
    Ok(true)
}

/// The object **keys** of an email's archived artifacts (SOUL §9/§28/§29): the raw
/// `.eml` (`raw_ref` is already a bare key) plus each attachment (whose `url` is a
/// `/storage/objects/<key>` path — strip the prefix). External-URL attachments (an
/// `http(s)://…` link we never archived) are skipped. Used by the deletion reconcile
/// to tear the archived objects down alongside the row.
fn archived_object_keys(email: &catalerum_core::Email) -> Vec<String> {
    const OBJECT_PREFIX: &str = "/storage/objects/";
    let mut keys = Vec::new();
    if let Some(raw_ref) = email
        .raw_ref
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        keys.push(raw_ref.to_string());
    }
    for att in &email.attachments {
        if let Some(key) = att
            .url
            .strip_prefix(OBJECT_PREFIX)
            .filter(|s| !s.is_empty())
        {
            keys.push(key.to_string());
        }
    }
    keys
}

/// Hard-delete a provider-deleted event's local row and enqueue the purge of its
/// derived `:Event` graph node — the reconcile-based `project_event` job finds the
/// row gone and detach-deletes the node (SOUL §6.3/§8). Idempotent like
/// [`delete_stored_email`]. The id is resolved *before* the delete because the
/// graph purge needs it and `EventRepo::delete_by_uid` returns only a bool.
async fn delete_stored_event(
    store: &Store,
    workspace_id: WorkspaceId,
    calendar_id: CalendarId,
    uid: &str,
) -> Result<bool> {
    let event_id = match store
        .events()
        .get_by_uid(workspace_id, calendar_id, uid)
        .await
    {
        Ok(e) => e.id,
        Err(catalerum_store::StoreError::NotFound) => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    if !store
        .events()
        .delete_by_uid(workspace_id, calendar_id, uid)
        .await?
    {
        return Ok(false);
    }
    if let Err(e) = crate::graph::enqueue_project_event(store, workspace_id, event_id).await {
        tracing::warn!(error = %e, event = %event_id,
            "collect: failed to enqueue deletion purge (project_event)");
    }
    tracing::debug!(%event_id, uid, "collect: reconciled upstream event deletion");
    Ok(true)
}

/// Reconcile removals for a **snapshot** email provider (Maildir): it re-emits its
/// full uid set on every changed poll and cannot emit `deletions`, so a stored
/// email absent from a complete snapshot was deleted at the source. The caller
/// guards this to a fully-drained, cursor-advancing, non-empty snapshot (mirroring
/// the on-demand sync path), so a no-change or partial read never prunes. Bounded:
/// one SQL listing of the mailbox's rows diffed against the snapshot's uid set —
/// both the size of the source the drain already held in memory.
async fn reconcile_email_snapshot(
    store: &Store,
    ctx: &AutomationContext,
    workspace_id: WorkspaceId,
    mailbox_id: MailboxId,
    live_uids: &BTreeSet<String>,
) -> Result<usize> {
    let stored = store
        .emails()
        .list_by_mailbox(workspace_id, mailbox_id, i64::MAX)
        .await?;
    let mut deleted = 0usize;
    for email in stored {
        if !live_uids.contains(&email.uid)
            && delete_stored_email(store, ctx, workspace_id, mailbox_id, &email.uid).await?
        {
            deleted += 1;
        }
    }
    Ok(deleted)
}

/// Reconcile removals for a **snapshot** calendar provider (local `.ics` /
/// webcal) — the event twin of [`reconcile_email_snapshot`], with the same guards
/// and bounds (the unbounded listing matches the sync path's reconcile: events
/// past any recency cap must still be prunable).
async fn reconcile_event_snapshot(
    store: &Store,
    workspace_id: WorkspaceId,
    calendar_id: CalendarId,
    live_uids: &BTreeSet<String>,
) -> Result<usize> {
    let stored = store
        .events()
        .list_by_workspace(
            workspace_id,
            Some(calendar_id),
            catalerum_store::DateRange::default(),
            i64::MAX,
        )
        .await?;
    let mut deleted = 0usize;
    for event in stored {
        if !live_uids.contains(&event.uid)
            && delete_stored_event(store, workspace_id, calendar_id, &event.uid).await?
        {
            deleted += 1;
        }
    }
    Ok(deleted)
}

/// Run one collected item's automation inline (its own [`AutomationRun`]) and
/// return whether the item is **committed** — i.e. whether the cursor may advance
/// over it. With `commit_on` unset the item is committed regardless (fire-and-forget).
/// With it set, the item is committed iff the named node `Succeeded`/`Skipped` (a
/// not-taken `Condition` `Skipped` counts as intentionally committed, SOUL §11). A
/// linear (non-graph) automation can't resolve a node id, so it falls back to the
/// whole run succeeding.
pub(crate) async fn run_item(
    store: &Store,
    ctx: &AutomationContext,
    workspace_id: WorkspaceId,
    automation: &catalerum_core::Automation,
    trigger_json: Value,
    commit_on: Option<&str>,
) -> Result<bool> {
    let run: AutomationRun = execute_for_job(
        store,
        ctx.runner().as_ref(),
        ctx.code().as_ref(),
        workspace_id,
        automation,
        Some(trigger_json),
        None,
    )
    .await?;
    committed_for(store, workspace_id, automation, &run, commit_on).await
}

/// Resolve the commit verdict for a finished run (see [`run_item`]).
async fn committed_for(
    store: &Store,
    workspace_id: WorkspaceId,
    automation: &catalerum_core::Automation,
    run: &AutomationRun,
    commit_on: Option<&str>,
) -> Result<bool> {
    let Some(node) = commit_on else {
        // Fire-and-forget: the cursor advances regardless of outcome (SOUL §11).
        return Ok(true);
    };
    let is_graph = Graph::from_spec(automation.spec.as_ref()).is_some();
    if !is_graph {
        // No node ids in a linear automation — fall back to the whole run.
        return Ok(run.status == RunStatus::Succeeded);
    }
    // The step whose recorded `action["node"]` is the commit_on target carries its
    // terminal status (the executor stamps the node id into each step's action JSON).
    let steps = store
        .automation_runs()
        .list_steps(workspace_id, run.id)
        .await?;
    for step in &steps {
        if step.action.get("node").and_then(Value::as_str) == Some(node) {
            return Ok(matches!(
                step.status,
                StepStatus::Succeeded | StepStatus::Skipped
            ));
        }
    }
    // The node never ran (an upstream failed before reaching it, or a never-taken
    // branch the executor didn't record) → uncommitted → re-collect next poll.
    Ok(false)
}

/// Advance a source's ledger after processing its batch (the contiguous-prefix
/// rule, SOUL §11/§29). The provider cursor only moves when the **whole** drained
/// batch committed (so an uncommitted/over-cap tail pins the cursor and is
/// re-pulled next poll); the committed-uid set is compacted differently per provider
/// mode to stay bounded:
/// - **incremental** (IMAP/JMAP/Gmail/CalDAV): once the cursor advances the
///   processed items are behind it and won't re-emit, so the set is cleared.
/// - **snapshot** (Maildir/local ics/webcal): the next changed snapshot re-emits
///   every uid, so the set is kept (to skip them) but pruned to the live source set
///   (dropping uids no longer present — i.e. deleted at the source).
fn advance_source(
    state: &mut SourceState,
    all_committed: bool,
    next_cursor: &Cursor,
    cursor_changed: bool,
    incremental: bool,
    batch_uids: &BTreeSet<String>,
) {
    state.initialized = true;
    if !all_committed {
        // Leave the cursor before the uncommitted/unprocessed tail; keep the
        // committed set so the succeeded items are skipped on the re-pull.
        return;
    }
    state.cursor = Some(next_cursor.0.clone());
    if incremental {
        // Behind the new cursor; deltas won't re-emit them, so the set is cleared.
        state.committed.clear();
    } else if cursor_changed {
        // A snapshot we actually re-read (its content hash changed): drop uids no
        // longer present (deleted at the source), keep the rest so the next re-emit
        // skips them.
        state.committed.retain(|uid| batch_uids.contains(uid));
    }
    // else: a snapshot poll that returned no change (empty re-list, cursor
    // unchanged) — keep the committed set intact. Pruning to an empty `batch_uids`
    // here would wrongly clear it and re-run every item on the next real change.
}

/// The first-poll backfill cutoff (SOUL §11/§29): items older than the cutoff are
/// dropped on the **first** poll so a large existing source doesn't flood. A
/// `backfill_window` of `{"days"|"hours"|"minutes": N}` sets the lookback; absent, a
/// first poll defaults to a short grace window (collect just-arrived mail, the
/// "newer than the automation" default), and a steady-state poll has no cutoff
/// (the cursor bounds it).
fn backfill_cutoff(
    backfill_window: Option<&Value>,
    initialized: bool,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    if let Some(window) = parse_window_minutes(backfill_window) {
        return Some(now - ChronoDuration::minutes(window));
    }
    if initialized {
        None
    } else {
        // First poll, no explicit window: a 1-day grace, so a freshly-wired source
        // collects very-recent mail without replaying its whole history.
        Some(now - ChronoDuration::days(1))
    }
}

/// Interpret a `backfill_window` predicate as a lookback in minutes: a bare number
/// (days), or `{"days"|"hours"|"minutes": N}`. Any other shape → `None` (use the
/// default). Negative/zero → `None`.
fn parse_window_minutes(window: Option<&Value>) -> Option<i64> {
    let minutes = match window? {
        Value::Number(n) => n.as_i64()?.checked_mul(24 * 60)?,
        Value::Object(map) => {
            if let Some(d) = map.get("days").and_then(Value::as_i64) {
                d.checked_mul(24 * 60)?
            } else if let Some(h) = map.get("hours").and_then(Value::as_i64) {
                h.checked_mul(60)?
            } else {
                map.get("minutes").and_then(Value::as_i64)?
            }
        }
        _ => return None,
    };
    (minutes > 0).then_some(minutes)
}

/// Whether an item with optional timestamp passes the backfill cutoff. An undated
/// item always passes (better to over-collect than silently drop new mail with no
/// `Date:` header).
fn passes_cutoff(at: Option<DateTime<Utc>>, cutoff: Option<DateTime<Utc>>) -> bool {
    match (at, cutoff) {
        (Some(t), Some(c)) => t >= c,
        _ => true,
    }
}

/// A stable, chronological sort key for in-order processing: dated items first by
/// timestamp, undated last, ties broken by uid.
fn sort_key(at: Option<DateTime<Utc>>, uid: &str) -> (i64, String) {
    (
        at.map(|t| t.timestamp()).unwrap_or(i64::MAX),
        uid.to_string(),
    )
}

/// Whether an email matches a `CollectEmail` trigger's optional `filter` — the same
/// interim object convention as [`calendar_event_filter_matches`]: optional
/// case-insensitive substring keys `"sender"` (the From address/name), `"subject"`,
/// `"body"` (the plain text); all supplied keys AND together. Absent / non-object →
/// no constraint.
fn email_filter_matches(filter: Option<&Value>, email: &Email) -> bool {
    let Some(obj) = filter.and_then(Value::as_object) else {
        return true;
    };
    let want = |key: &str| obj.get(key).and_then(Value::as_str);
    let from = email.from.as_ref().map(|a| match &a.name {
        Some(n) => format!("{n} <{}>", a.address),
        None => a.address.clone(),
    });
    contains_ci(want("sender"), from.as_deref())
        && contains_ci(want("subject"), Some(&email.subject))
        && contains_ci(want("body"), email.body_text.as_deref())
}

/// A case-insensitive substring filter: `None` filter matches anything; a `Some(f)`
/// requires the candidate present and containing `f`.
fn contains_ci(filter: Option<&str>, candidate: Option<&str>) -> bool {
    match filter {
        None => true,
        Some(f) => {
            candidate.is_some_and(|c| c.to_ascii_lowercase().contains(&f.to_ascii_lowercase()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use catalerum_core::id::MailboxId;
    use catalerum_core::model::EmailAddress;

    fn email(uid: &str, received: Option<DateTime<Utc>>) -> Email {
        Email {
            id: catalerum_core::id::EmailId::new(),
            workspace_id: WorkspaceId::new(),
            mailbox_id: MailboxId::new(),
            uid: uid.to_string(),
            message_id: None,
            from: Some(EmailAddress {
                name: Some("Ada".into()),
                address: "ada@bank.com".into(),
            }),
            to: vec![],
            cc: vec![],
            subject: "Your statement".into(),
            received_at: received,
            body_text: Some("a refund is enclosed".into()),
            body_html: None,
            has_attachments: false,
            flags: vec![],
            labels: vec![],
            raw_ref: None,
            attachments: Vec::new(),
            raw: None,
        }
    }

    #[test]
    fn ledger_round_trips_and_tolerates_foreign_tokens() {
        let mut l = CollectLedger::default();
        let mut s = SourceState {
            cursor: Some("p3".into()),
            ..Default::default()
        };
        s.committed.insert("u1".into());
        s.committed.insert("u2".into());
        s.initialized = true;
        l.sources.insert("INBOX".into(), s);
        let encoded = l.encode();
        let back = CollectLedger::decode(Some(&encoded));
        assert_eq!(back, l, "ledger round-trips through sync_token");

        // A foreign token (an old CursorMap) decodes to an empty ledger, not a panic.
        let foreign = Cursor::new(r#"{"INBOX":"sha256:abc"}"#);
        assert_eq!(
            CollectLedger::decode(Some(&foreign)),
            CollectLedger::default()
        );
        assert_eq!(CollectLedger::decode(None), CollectLedger::default());
    }

    #[test]
    fn advance_only_moves_cursor_when_the_whole_batch_committed() {
        let batch: BTreeSet<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();

        // Incomplete batch → cursor stays, committed set retained, initialized set.
        let mut state = SourceState::default();
        state.committed.insert("a".into());
        advance_source(&mut state, false, &Cursor::new("p2"), true, true, &batch);
        assert_eq!(state.cursor, None, "uncommitted tail pins the cursor");
        assert!(
            state.committed.contains("a"),
            "succeeded items kept for skip-on-repull"
        );
        assert!(state.initialized);

        // Complete + incremental → cursor advances, committed cleared (behind cursor).
        let mut state = SourceState::default();
        state.committed.insert("a".into());
        advance_source(&mut state, true, &Cursor::new("p2"), true, true, &batch);
        assert_eq!(state.cursor.as_deref(), Some("p2"));
        assert!(
            state.committed.is_empty(),
            "incremental: cleared once behind the cursor"
        );

        // Complete + snapshot that actually re-read (cursor changed) → cursor
        // advances, committed pruned to the live set.
        let mut state = SourceState::default();
        state.committed.insert("a".into()); // still in the snapshot
        state.committed.insert("gone".into()); // deleted at source
        advance_source(&mut state, true, &Cursor::new("h2"), true, false, &batch);
        assert_eq!(state.cursor.as_deref(), Some("h2"));
        assert!(
            state.committed.contains("a"),
            "snapshot: kept (re-emits next change)"
        );
        assert!(
            !state.committed.contains("gone"),
            "snapshot: pruned a vanished uid"
        );
    }

    #[test]
    fn snapshot_no_change_poll_keeps_committed_set() {
        // The double-run regression: on a no-change snapshot poll the provider
        // returns empty `upserts` (so batch_uids is empty) with the SAME cursor.
        // advance_source must NOT prune (which would clear the set and re-run every
        // item on the next real change).
        let empty: BTreeSet<String> = BTreeSet::new();
        let mut state = SourceState {
            cursor: Some("h1".into()),
            initialized: true,
            ..Default::default()
        };
        state.committed.insert("a".into());
        state.committed.insert("b".into());
        // cursor_changed = false (next_cursor == prior), snapshot (incremental=false).
        advance_source(&mut state, true, &Cursor::new("h1"), false, false, &empty);
        assert_eq!(state.cursor.as_deref(), Some("h1"));
        assert!(
            state.committed.contains("a") && state.committed.contains("b"),
            "a no-change snapshot poll must keep the committed set, not wipe it"
        );
    }

    #[test]
    fn backfill_cutoff_bounds_first_poll_only() {
        let now = DateTime::from_timestamp(1_000_000_000, 0).unwrap();
        // Explicit window applies on any poll.
        let c = backfill_cutoff(Some(&json!({ "days": 30 })), true, now);
        assert_eq!(c, Some(now - ChronoDuration::days(30)));
        // Bare number = days.
        assert_eq!(
            backfill_cutoff(Some(&json!(7)), false, now),
            Some(now - ChronoDuration::days(7))
        );
        // First poll, no window → a 1-day grace; steady state → no cutoff.
        assert_eq!(
            backfill_cutoff(None, false, now),
            Some(now - ChronoDuration::days(1))
        );
        assert_eq!(backfill_cutoff(None, true, now), None);
        // Garbage / non-positive → treated as no explicit window.
        assert_eq!(backfill_cutoff(Some(&json!(0)), true, now), None);
        assert_eq!(backfill_cutoff(Some(&json!("30d")), true, now), None);
    }

    #[test]
    fn passes_cutoff_drops_old_keeps_undated() {
        let now = DateTime::from_timestamp(1_000_000_000, 0).unwrap();
        let cutoff = Some(now - ChronoDuration::days(1));
        assert!(passes_cutoff(Some(now), cutoff), "recent passes");
        assert!(
            !passes_cutoff(Some(now - ChronoDuration::days(2)), cutoff),
            "old dropped"
        );
        assert!(passes_cutoff(None, cutoff), "undated always passes");
        assert!(
            passes_cutoff(Some(now - ChronoDuration::days(99)), None),
            "no cutoff → all pass"
        );
    }

    #[test]
    fn email_filter_matches_sender_subject_body() {
        let e = email("u1", None);
        assert!(email_filter_matches(None, &e), "no filter matches");
        assert!(email_filter_matches(
            Some(&json!({ "sender": "BANK.com" })),
            &e
        ));
        assert!(email_filter_matches(
            Some(&json!({ "subject": "statement" })),
            &e
        ));
        assert!(email_filter_matches(Some(&json!({ "body": "REFUND" })), &e));
        // AND of supplied keys: a sender match + a subject miss → no match.
        assert!(!email_filter_matches(
            Some(&json!({ "sender": "bank.com", "subject": "invoice" })),
            &e
        ));
        // A non-object filter imposes no constraint.
        assert!(email_filter_matches(Some(&json!("nope")), &e));
    }
}
