//! The clock scheduler for time-driven `Schedule { cron }` triggers (SOUL §11).
//!
//! The push event sources (a Kanban move §24, a webhook) enqueue a
//! `run_automation` job the moment they fire. A `Schedule` trigger has no event —
//! it fires on a clock — so a [`ScheduleWorker`] ticks periodically and, for each
//! enabled `Schedule` automation whose cron became due since the last tick,
//! enqueues the same durable `run_automation` job. The dispatch tail (worker →
//! [`crate::run_automation`] → the §19-scoped runner) is shared with every other
//! source; this module is only the *when*.
//!
//! **No catch-up across restarts.** `last_tick` starts at process start and only
//! moves forward, so a cron fires **only while the scheduler is running** — a fire
//! missed during downtime is skipped, never replayed. Single-fire across pods (the
//! §11 Valkey lock) and durable catch-up are a later slice; until then this assumes
//! the single-pod dev dispatch path (SOUL §6.2) — under multi-pod, each pod's
//! scheduler would enqueue a tick, the same at-least-once property the
//! `run_automation` job already carries.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use catalerum_automation::{calendar_event_filter_matches, due_occurrence, Trigger};
use catalerum_bus::{Bus, DistLock};
use catalerum_core::Automation;
use catalerum_graph::{GraphStore, WorkspaceFacts, MAX_WORKSPACE_EDGES, MAX_WORKSPACE_NODES};
use catalerum_store::{DateRange, Store};

use crate::automation::enqueue_run_automation;
use crate::collect::enqueue_collect;
use crate::error::Result;

/// Default scheduler tick — once a minute, matching cron's 1-minute granularity.
const DEFAULT_TICK: Duration = Duration::from_secs(60);

/// How long a single-fire claim is held. Generous vs. inter-pod clock skew /
/// tick lag (so no second pod re-fires the same occurrence), yet keyed per
/// occurrence-instant so it never blocks the *next* occurrence (a different key).
const FIRE_LOCK_TTL: Duration = Duration::from_secs(300);

/// Scan every workspace's enabled `Schedule` automations and enqueue a
/// `run_automation` job for each whose cron fired in the half-open window
/// `(after, now]` (SOUL §11). Returns the enqueued job ids. An automation with
/// several schedule triggers fires **once** per window; a malformed cron is
/// skipped with a warning, never failing the scan; the firing cron + occurrence
/// are recorded on the run's trigger for audit.
///
/// **Single-fire across pods (SOUL §11/§6.2):** before enqueuing, the occurrence
/// is claimed with a `lock` keyed by `(automation, fire-instant)` — the same
/// pending occurrence yields the same key on every pod, so exactly one pod
/// enqueues it. The claim is left to expire (TTL), not released, so a losing
/// pod's later scan still sees it taken. With the in-process lock this is a no-op
/// for single-pod dev; with Valkey it makes a multi-pod scheduler single-fire.
pub async fn scan_schedules(
    store: &Store,
    lock: &dyn DistLock,
    after: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<Vec<uuid::Uuid>> {
    let mut jobs = Vec::new();
    for workspace in store.workspaces().list().await? {
        for automation in store.automations().list_by_workspace(workspace.id).await? {
            if !automation.enabled {
                continue;
            }
            let Some((cron, fire)) = first_due(&automation, after, now) else {
                continue;
            };
            // Claim this exact occurrence; another pod that already claimed it wins.
            let key = format!("automation-fire:{}:{}", automation.id, fire.timestamp());
            match lock.try_acquire(&key, FIRE_LOCK_TTL).await {
                Ok(Some(_guard)) => {} // claimed — fall through to enqueue
                Ok(None) => continue,  // another pod fired this occurrence
                Err(e) => {
                    warn!(automation = %automation.id, error = %e, "fire-lock error; skipping to avoid a double-fire");
                    continue;
                }
            }
            let trigger = json!({
                "kind": "schedule", "cron": cron, "fired_at": fire.to_rfc3339(),
            });
            jobs.push(
                enqueue_run_automation(store, workspace.id, automation.id, Some(trigger)).await?,
            );
        }
    }
    Ok(jobs)
}

/// Scan every workspace's enabled `CalendarEvent` automations and enqueue a
/// `run_automation` job for each upcoming event whose **lead instant**
/// (`start − lead`) falls in this tick's window (SOUL §11/§8) — i.e. it just became
/// time to fire the "N minutes before the meeting" reminder. Returns the enqueued
/// job ids. The firing event + lead are recorded on the run's trigger for audit
/// (and seed an `LlmAgent` action).
///
/// **Window.** The events query's `[from, to)` over `start` (`start >= from AND
/// start < to`) with `from = after + lead`, `to = now + lead` fires events whose
/// lead instant `start − lead ∈ [after, now)` — left-closed, right-open. Consecutive
/// ticks (`[t0,t1)` then `[t1,t2)`) partition the timeline with no gaps or overlaps,
/// so each event is covered by **exactly one** tick. (This mirrors the cron
/// scanner; the half-open *side* differs from its `(after, now]`, but the partition
/// property — every instant in exactly one window — is the same.)
///
/// **Single-fire (SOUL §11/§6.2).** Each occurrence is claimed via `lock` keyed by
/// `(automation, event, lead-instant)` — exactly as [`scan_schedules`] keys by the
/// cron fire-instant. So an identical occurrence single-fires across pods, while a
/// **rescheduled** event (new `start`) or a **changed lead** yields a *new*
/// lead-instant → a fresh, correct reminder rather than a permanently-suppressed
/// one. The claim is left to TTL-expire, never released.
///
/// **Filter (SOUL §8/§11).** The trigger's opaque `filter` is evaluated per event
/// via [`calendar_event_filter_matches`] — an optional case-insensitive
/// summary/location/description substring predicate (AND of the supplied keys);
/// an absent/non-object filter imposes no constraint, so **every** event in the
/// window fires (backward-compatible with the inert-field era). The gate runs
/// *before* the single-fire claim, so a non-matching event never consumes a lock.
///
/// **Interim scope.** Only the **first** `CalendarEvent` trigger's lead + filter is
/// used per automation; and a lead beyond [`MAX_LEAD_MINUTES`] is skipped with a
/// warning (never panicking the scheduler).
pub async fn scan_calendar_event_triggers(
    store: &Store,
    lock: &dyn DistLock,
    after: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<Vec<uuid::Uuid>> {
    let mut jobs = Vec::new();
    for workspace in store.workspaces().list().await? {
        for automation in store.automations().list_by_workspace(workspace.id).await? {
            if !automation.enabled {
                continue;
            }
            let Some((lead_minutes, filter)) = first_calendar_trigger(&automation) else {
                continue;
            };
            // `lead_minutes` is capped to ±MAX_LEAD_MINUTES (~1y), so neither the
            // Duration nor the `start ± lead` datetime arithmetic can overflow-panic.
            let lead = chrono::Duration::minutes(lead_minutes);
            // Events whose start crosses the lead point in this window (see the
            // `[from, to)` partition note above). Workspace-scoped; all calendars.
            let range = DateRange {
                from: Some(after + lead),
                to: Some(now + lead),
            };
            // The `[after, now)`-derived window is one scheduler tick wide, so it
            // naturally holds few events; the cap is just a defensive ceiling.
            let events = store
                .events()
                .list_by_workspace(
                    workspace.id,
                    None,
                    range,
                    catalerum_store::DEFAULT_EVENT_LIMIT,
                )
                .await?;
            for event in events {
                // Gate on the trigger's `filter` before claiming a single-fire lock
                // (so a non-matching event never consumes a claim): a
                // summary/location/description substring predicate, or no constraint
                // when absent (SOUL §8/§11). Evaluated here — the scheduler, not
                // `Trigger::matches` — because a calendar trigger fires on a clock.
                if !calendar_event_filter_matches(
                    filter.as_ref(),
                    &event.summary,
                    event.location.as_deref(),
                    event.body.as_deref(),
                ) {
                    continue;
                }
                // Claim this exact (automation, event, lead-instant). Keying on the
                // fire-instant (not just the event id) means a moved meeting / changed
                // lead re-fires at its new instant, while the same occurrence on
                // another pod collides on the same key → single-fire.
                let lead_instant = event.start - lead;
                let key = format!(
                    "calendar-event-fire:{}:{}:{}",
                    automation.id,
                    event.id,
                    lead_instant.timestamp()
                );
                match lock.try_acquire(&key, FIRE_LOCK_TTL).await {
                    Ok(Some(_guard)) => {} // claimed — fall through to enqueue
                    Ok(None) => continue,  // another pod fired this (automation, event)
                    Err(e) => {
                        warn!(automation = %automation.id, error = %e, "fire-lock error; skipping to avoid a double-fire");
                        continue;
                    }
                }
                let trigger = json!({
                    "kind": "calendar_event",
                    "event_id": event.id,
                    "summary": event.summary,
                    "starts_at": event.start.to_rfc3339(),
                    "lead_minutes": lead_minutes,
                    "fired_at": now.to_rfc3339(),
                });
                jobs.push(
                    enqueue_run_automation(store, workspace.id, automation.id, Some(trigger))
                        .await?,
                );
            }
        }
    }
    Ok(jobs)
}

/// The **lead** (in minutes before the event start) and the opaque **filter** of
/// the first `CalendarEvent` trigger on `automation`, or `None` if it has no
/// calendar trigger or the lead is uninterpretable (logged + skipped — never
/// silently fires at the wrong time). The `filter` is carried so the event scan
/// can gate which events fire (see [`calendar_event_filter_matches`]).
fn first_calendar_trigger(automation: &Automation) -> Option<(i64, Option<Value>)> {
    automation.triggers.iter().find_map(|t| {
        match serde_json::from_value::<Trigger>(t.clone()) {
            Ok(Trigger::CalendarEvent { lead, filter }) => match parse_lead_minutes(lead.as_ref()) {
                Some(m) => Some((m, filter)),
                None => {
                    warn!(automation = %automation.id, "skipping calendar_event trigger with an uninterpretable lead");
                    None
                }
            },
            _ => None,
        }
    })
}

/// The widest accepted lead (~1 year). Caps an absurd/garbage `lead` so the
/// downstream `chrono::Duration` + `DateTime` arithmetic can never overflow-panic
/// and bring down the whole scheduler task (a `lead` beyond this is a config error,
/// not a real "remind me before" intent).
const MAX_LEAD_MINUTES: i64 = 366 * 24 * 60;

/// Interpret a `CalendarEvent` trigger's opaque `lead` as **minutes before the
/// event start**: a bare number (`"lead": 10`), an object `{ "minutes": 10 }`, or
/// an absent/null lead (→ `0`, fire at start). Any other shape — or a magnitude
/// beyond [`MAX_LEAD_MINUTES`] — yields `None` (the trigger is skipped, never
/// silently firing at the wrong instant nor panicking on overflow). This is the
/// minimal interpretation until the general predicate language lands (§11).
fn parse_lead_minutes(lead: Option<&Value>) -> Option<i64> {
    let minutes = match lead {
        None | Some(Value::Null) => 0,
        Some(Value::Number(n)) => n.as_i64()?,
        Some(Value::Object(map)) => map.get("minutes").and_then(Value::as_i64)?,
        _ => return None,
    };
    // `unsigned_abs` (not `abs`) so `i64::MIN` itself can't overflow-panic here.
    (minutes.unsigned_abs() <= MAX_LEAD_MINUTES as u64).then_some(minutes)
}

/// Default poll interval for a `GraphQuery` whose `every` is absent.
const DEFAULT_GRAPH_EVERY_MINUTES: i64 = 5;
/// Widest accepted `every` (1 year) — guards arithmetic + absurd config.
const MAX_GRAPH_EVERY_MINUTES: i64 = 365 * 24 * 60;
/// Row count at which a `GraphQuery` poll result is flagged as suspiciously large.
/// The poll only needs **existence** (`is_empty`), so a result this big means the
/// operator's Datalog goal is over-broad — it buffers the whole result into memory
/// each tick and often signals a §18 scoping smell. We don't truncate (correctness
/// of the existence check is unchanged); we surface it so it can't grow silently.
const GRAPH_QUERY_ROWS_WARN: usize = 10_000;

/// Wall-clock backstop for evaluating one `GraphQuery` Datalog program. Evaluation
/// is pure and structurally terminating (SOUL §6.3), so this only bounds a
/// pathological program; the evaluator also enforces its own deadline.
const GRAPH_QUERY_EVAL_TIMEOUT: Duration = Duration::from_secs(5);

/// Default collect poll cadence when a `CollectEmail`/`CollectCalendar` trigger's
/// `every` is **unset** — the shared scheduler tick ([`DEFAULT_TICK`], 60s): an
/// unset collect trigger polls once per tick. This resolves the SOUL §29 "Collect
/// cadence" open question: **per-trigger `every` with the shared tick as default**.
const DEFAULT_COLLECT_EVERY_SECS: i64 = DEFAULT_TICK.as_secs() as i64;
/// Minimum collect cadence. The scheduler only wakes every [`DEFAULT_TICK`] (60s),
/// so a source cannot be polled faster than the tick — a smaller `every` **clamps
/// up** to this floor rather than erroring (SOUL §29: "too-small values clamp").
const MIN_COLLECT_EVERY_SECS: i64 = DEFAULT_TICK.as_secs() as i64;
/// Widest accepted collect cadence (1 year) — guards the bucket arithmetic + absurd
/// config; a larger `every` **clamps down** to this rather than erroring.
const MAX_COLLECT_EVERY_SECS: i64 = 365 * 24 * 60 * 60;

/// Build a `catalerum_logic::Facts` EDB from a loaded [`WorkspaceFacts`] slice.
fn facts_from(wf: &WorkspaceFacts) -> catalerum_logic::Facts {
    let mut facts = catalerum_logic::Facts::new();
    for (id, label) in &wf.nodes {
        facts.node(id.as_str(), label.as_str());
    }
    for (from, ty, to) in &wf.edges {
        facts.edge(from.as_str(), ty.as_str(), to.as_str());
    }
    for (id, key, value) in &wf.props {
        facts.prop(id.as_str(), key.as_str(), value.as_str());
    }
    facts
}

/// Evaluate a Datalog `program` over `facts` off the async runtime — the evaluator
/// is pure and synchronous (`!Send`-free plain data), so it runs in
/// [`tokio::task::spawn_blocking`] under a [`tokio::time::timeout`] backstop. Errors
/// (parse/eval budget/deadline/join) are returned as a reason string to log + skip.
async fn run_datalog(
    program: catalerum_logic::Program,
    facts: catalerum_logic::Facts,
) -> std::result::Result<catalerum_logic::EvalOutput, String> {
    let limits = catalerum_logic::EvalLimits::with_deadline(GRAPH_QUERY_EVAL_TIMEOUT);
    let handle =
        tokio::task::spawn_blocking(move || catalerum_logic::eval(&program, &facts, &limits));
    match tokio::time::timeout(GRAPH_QUERY_EVAL_TIMEOUT + Duration::from_secs(1), handle).await {
        Ok(Ok(Ok(out))) => Ok(out),
        Ok(Ok(Err(e))) => Err(e.to_string()),
        Ok(Err(join)) => Err(format!("eval task panicked: {join}")),
        Err(_elapsed) => Err("eval timed out".to_string()),
    }
}

/// Scan every workspace's enabled `GraphQuery` automations and, for each whose
/// `every`-minute poll boundary fell in this tick's window, run its Cypher against
/// Neo4j and enqueue a `run_automation` job **when the query returns at least one
/// row** (SOUL §11/§6.3) — the periodic "fire while this graph condition holds"
/// source. Returns the enqueued job ids.
///
/// **Poll cadence.** `every` (minutes) buckets the timeline into `N`-minute spans;
/// the automation fires once when `now` crosses into a new bucket since `after`
/// (stateless, like the cron window). The bucket boundary is the fire instant, used
/// to single-fire across pods via `lock` keyed `(automation, fire-instant)` —
/// exactly as [`scan_schedules`]. The query is re-run **each** such interval, so it
/// fires repeatedly while the condition holds (a monitoring poll, not edge-trigger).
///
/// **Scoping (§18).** Only the owning workspace's facts are loaded (a fixed,
/// structurally-scoped read) and the Datalog program is evaluated in-process over
/// them; the language cannot name a workspace, so cross-tenant reach is impossible
/// by construction. **Resilience.** A parse/load/eval error is logged and skipped —
/// one bad query never fails the scan or starves other automations.
pub async fn scan_graph_queries(
    store: &Store,
    graph: &GraphStore,
    lock: &dyn DistLock,
    after: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<Vec<uuid::Uuid>> {
    let mut jobs = Vec::new();
    for workspace in store.workspaces().list().await? {
        for automation in store.automations().list_by_workspace(workspace.id).await? {
            if !automation.enabled {
                continue;
            }
            let Some((query, every_minutes)) = first_graph_query(&automation) else {
                continue;
            };
            let Some(fire) = graph_due(every_minutes, after, now) else {
                continue;
            };
            // Single-fire this poll occurrence across pods (left to TTL-expire).
            let key = format!("graph-query-fire:{}:{}", automation.id, fire.timestamp());
            match lock.try_acquire(&key, FIRE_LOCK_TTL).await {
                Ok(Some(_guard)) => {} // claimed — fall through to poll
                Ok(None) => continue,  // another pod is polling this occurrence
                Err(e) => {
                    warn!(automation = %automation.id, error = %e, "fire-lock error; skipping to avoid a double-poll");
                    continue;
                }
            }
            // Parse the Datalog program (already validated at authoring and in
            // `first_graph_query`; parse again for the executable program).
            let program = match catalerum_logic::parse(&query) {
                Ok(p) => p,
                Err(e) => {
                    warn!(automation = %automation.id, error = %e, "graph_query Datalog failed to parse; skipping this poll");
                    continue;
                }
            };
            // Load only this workspace's facts (a fixed, structurally-scoped read —
            // no query text ever reaches Neo4j, so cross-workspace reach is
            // impossible by construction, §18), then evaluate the program over them.
            let facts = match graph
                .load_workspace_facts(workspace.id, MAX_WORKSPACE_NODES, MAX_WORKSPACE_EDGES)
                .await
            {
                Ok(wf) => {
                    if wf.truncated {
                        warn!(automation = %automation.id, "graph_query ran over a workspace that hit the fact cap; the poll saw a partial graph (§18)");
                    }
                    facts_from(&wf)
                }
                Err(e) => {
                    warn!(automation = %automation.id, error = %e, "loading graph facts failed; skipping this poll");
                    continue;
                }
            };
            let out = match run_datalog(program, facts).await {
                Ok(o) => o,
                Err(reason) => {
                    warn!(automation = %automation.id, %reason, "graph_query evaluation failed; skipping this poll");
                    continue;
                }
            };
            // The poll only needs existence; a very large result means an over-broad
            // program (§11/§18). Surface it (don't truncate — existence is unaffected).
            if out.rows.len() >= GRAPH_QUERY_ROWS_WARN {
                warn!(
                    automation = %automation.id,
                    rows = out.rows.len(),
                    "graph_query poll returned a very large result; it only needs to know if any \
                     rows exist — narrow the query to bound memory (§11/§18)"
                );
            }
            // Non-empty → the condition holds → fire. Empty → polled, nothing to do.
            if out.rows.is_empty() {
                continue;
            }
            let trigger = json!({
                "kind": "graph_query",
                "query": query,
                "rows": out.rows.len(),
                "fired_at": fire.to_rfc3339(),
            });
            jobs.push(
                enqueue_run_automation(store, workspace.id, automation.id, Some(trigger)).await?,
            );
        }
    }
    Ok(jobs)
}

/// Scan every workspace's enabled `CollectEmail` / `CollectCalendar` automations
/// and, for each whose per-trigger `every`-second poll boundary fell in this tick's
/// window, enqueue a durable **collect job** (SOUL §10/§28) — the head of a
/// user-authored ingest graph. Returns the enqueued job ids.
///
/// Unlike the other scanners, this does **not** enqueue `run_automation`: a collect
/// job does heavy provider I/O (off this 60s clock) and itself fans out one
/// `AutomationRun` per new external item (see [`crate::collect`]). The scheduler is
/// only the *when*.
///
/// **Cadence (SOUL §29).** Each collect trigger carries an optional `every`; unset →
/// the shared tick ([`DEFAULT_COLLECT_EVERY_SECS`]), set → that trigger's own cadence
/// (clamped to `[MIN_COLLECT_EVERY_SECS, MAX_COLLECT_EVERY_SECS]`). Like every other
/// scanner the cadence is **stateless deterministic bucketing** over `(after, now]` —
/// no per-trigger last-poll state is kept; the loop re-lists automations each tick and
/// the bucket boundary is derived from wall-clock, so it is the deterministic
/// fire-instant claimed via `lock` keyed `(automation, fire-instant)` for single-fire
/// across pods. A collect automation with several collect triggers enqueues for the
/// **first** one.
pub async fn scan_collect_triggers(
    store: &Store,
    lock: &dyn DistLock,
    after: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<Vec<uuid::Uuid>> {
    let mut jobs = Vec::new();
    for workspace in store.workspaces().list().await? {
        for automation in store.automations().list_by_workspace(workspace.id).await? {
            if !automation.enabled {
                continue;
            }
            let Some((trigger, every_secs)) = first_collect_trigger(&automation) else {
                continue;
            };
            let Some(fire) = due_bucket(every_secs, after, now) else {
                continue;
            };
            // Single-fire this poll occurrence across pods (left to TTL-expire).
            let key = format!("collect-fire:{}:{}", automation.id, fire.timestamp());
            match lock.try_acquire(&key, FIRE_LOCK_TTL).await {
                Ok(Some(_guard)) => {} // claimed — fall through to enqueue
                Ok(None) => continue,  // another pod enqueued this occurrence
                Err(e) => {
                    warn!(automation = %automation.id, error = %e, "fire-lock error; skipping to avoid a double-collect");
                    continue;
                }
            }
            match enqueue_collect(store, workspace.id, automation.id, &trigger).await {
                Ok(id) => jobs.push(id),
                Err(e) => {
                    warn!(automation = %automation.id, error = %e, "failed to enqueue collect job")
                }
            }
        }
    }
    Ok(jobs)
}

/// The first `CollectEmail`/`CollectCalendar` trigger on `automation` (with its
/// resolved poll cadence in **seconds**), or `None` if it has none, its `every` is
/// uninterpretable, or its `connection` is not a connection id (each logged +
/// skipped — never silently polling at the wrong cadence, and never enqueuing a
/// job that can only ever fail: a placeholder like `"fastmail"` written before
/// authoring-time validation existed would otherwise spam a doomed collect job
/// every tick).
fn first_collect_trigger(automation: &Automation) -> Option<(Trigger, i64)> {
    automation.triggers.iter().find_map(|t| {
        let trigger = serde_json::from_value::<Trigger>(t.clone()).ok()?;
        if !trigger.is_collect() {
            return None;
        }
        let connection = trigger.collect_connection().unwrap_or_default().trim();
        if connection.parse::<catalerum_core::ConnectionId>().is_err() {
            warn!(
                automation = %automation.id,
                connection,
                "skipping collect trigger whose `connection` is not a connection id \
                 (fix the automation to reference an existing connection's uuid)"
            );
            return None;
        }
        let secs = match parse_collect_every_secs(trigger.collect_every()) {
            Some(s) => s,
            None => {
                warn!(automation = %automation.id, "skipping collect trigger with an uninterpretable `every`");
                return None;
            }
        };
        Some((trigger, secs))
    })
}

/// Interpret a collect trigger's opaque `every` as a poll cadence in **seconds**
/// (SOUL §29 "Collect cadence"). Accepted shapes:
/// - absent / null → [`DEFAULT_COLLECT_EVERY_SECS`] (the shared tick).
/// - a bare integer `N` → `N` **minutes** — the codebase's `every`/`lead` convention
///   (shared with [`parse_every_minutes`]/[`parse_lead_minutes`]), i.e. `N*60` seconds.
/// - `{ "minutes": N }` → `N` minutes; `{ "seconds": N }` → `N` seconds (sub-minute
///   resolution, taking precedence when both keys are present).
/// - a duration **string** with unit suffixes: `"30s"`, `"5m"`, `"1h"`, `"1h30m"`,
///   `"2d"` (see [`parse_duration_secs`]).
///
/// The result is **clamped** to `[MIN_COLLECT_EVERY_SECS, MAX_COLLECT_EVERY_SECS]`: a
/// too-small cadence (below the scheduler tick) clamps **up** to the floor and a
/// too-large one clamps **down** — an out-of-range config is honored at the nearest
/// sane bound rather than skipping the source (SOUL §29). Only a shape that can't be
/// interpreted at all yields `None` (the trigger is skipped + warned).
fn parse_collect_every_secs(every: Option<&Value>) -> Option<i64> {
    let secs = match every {
        None | Some(Value::Null) => DEFAULT_COLLECT_EVERY_SECS,
        // A bare number is minutes (the shared `every`/`lead` convention); saturate
        // rather than overflow-panic so an absurd value clamps to the ceiling below.
        Some(Value::Number(n)) => n.as_i64()?.saturating_mul(60),
        Some(Value::Object(map)) => {
            if let Some(s) = map.get("seconds") {
                s.as_i64()?
            } else {
                let m = map.get("minutes")?;
                m.as_i64()?.saturating_mul(60)
            }
        }
        Some(Value::String(s)) => parse_duration_secs(s)?,
        _ => return None,
    };
    Some(secs.clamp(MIN_COLLECT_EVERY_SECS, MAX_COLLECT_EVERY_SECS))
}

/// Parse a compact duration string into **seconds** (SOUL §29 collect cadence): a run
/// of `<integer><unit>` chunks where unit ∈ `s`/`m`/`h`/`d`/`w`/`y`
/// (seconds/minutes/hours/days/weeks/years; a year is 365 days), e.g. `"30s"`, `"5m"`,
/// `"1h"`, `"1h30m"`, `"2d"`, `"1w"`, `"1y"` (`1y` is the advertised max cadence —
/// accepting it keeps this in step with the web editor's `is_duration_string` shape
/// check, which already allows `y`).
/// ASCII, case-insensitive; surrounding whitespace is ignored. A **unit-less** string
/// (e.g. `"300"`) is rejected — a bare integer must be a JSON number (which the collect
/// parser reads as *minutes*), so the two forms never silently disagree. Empty, an
/// unknown/duplicated unit, a unit with no number, or arithmetic overflow yields `None`.
fn parse_duration_secs(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let mut total: i64 = 0;
    let mut num: Option<i64> = None;
    let mut saw_chunk = false;
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            let d = i64::from(ch as u8 - b'0');
            num = Some(num.unwrap_or(0).checked_mul(10)?.checked_add(d)?);
        } else {
            let unit_secs = match ch.to_ascii_lowercase() {
                's' => 1,
                'm' => 60,
                'h' => 3_600,
                'd' => 86_400,
                'w' => 604_800,
                'y' => 31_536_000, // 365 days
                _ => return None,  // unknown unit / stray char
            };
            // A unit with no preceding number ("m", "5mh") is invalid.
            let n = num.take()?;
            total = total.checked_add(n.checked_mul(unit_secs)?)?;
            saw_chunk = true;
        }
    }
    // A trailing number with no unit ("5m30") or a string with no unit at all ("300").
    if num.is_some() || !saw_chunk {
        return None;
    }
    Some(total)
}

/// The `(query, every-minutes)` of the first `GraphQuery` trigger on `automation`,
/// or `None` if it has none or its `every` is uninterpretable (logged + skipped). A
/// legacy trigger still using the retired raw-Cypher `cypher` field no longer
/// decodes; it is warned about and skipped (never crashes the scan).
fn first_graph_query(automation: &Automation) -> Option<(String, i64)> {
    automation.triggers.iter().find_map(|t| {
        let (query, every) = match serde_json::from_value::<Trigger>(t.clone()) {
            Ok(Trigger::GraphQuery { query, every }) => (query, every),
            Ok(_) => return None, // some other trigger kind
            Err(_) => {
                // A retired raw-Cypher `graph_query` (uses the removed `cypher` field)
                // no longer decodes — warn + skip so it can be re-authored as Datalog.
                if t.get("kind").and_then(Value::as_str) == Some("graph_query") {
                    warn!(automation = %automation.id, "skipping a graph_query trigger that uses the retired `cypher` field — re-author it as a Datalog `query` (SOUL §6.3/§18)");
                }
                return None;
            }
        };
        // Re-validate at poll time too (not just at authoring), so an unsafe/invalid
        // program is never evaluated even if one slipped into storage.
        if let Err(reason) = catalerum_logic::validate(&query) {
            warn!(automation = %automation.id, %reason, "skipping invalid graph_query Datalog (not run)");
            return None;
        }
        match parse_every_minutes(every.as_ref()) {
            Some(m) => Some((query, m)),
            None => {
                warn!(automation = %automation.id, "skipping graph_query trigger with an uninterpretable `every`");
                None
            }
        }
    })
}

/// Interpret a `GraphQuery` trigger's opaque `every` as a poll interval in
/// **minutes**: a bare number, `{ "minutes": N }`, or absent/null →
/// [`DEFAULT_GRAPH_EVERY_MINUTES`]. Must be in `1..=`[`MAX_GRAPH_EVERY_MINUTES`];
/// any other shape or an out-of-range value yields `None` (the trigger is skipped).
fn parse_every_minutes(every: Option<&Value>) -> Option<i64> {
    let minutes = match every {
        None | Some(Value::Null) => DEFAULT_GRAPH_EVERY_MINUTES,
        Some(Value::Number(n)) => n.as_i64()?,
        Some(Value::Object(map)) => map.get("minutes").and_then(Value::as_i64)?,
        _ => return None,
    };
    (1..=MAX_GRAPH_EVERY_MINUTES)
        .contains(&minutes)
        .then_some(minutes)
}

/// The poll fire-instant for a `span_secs`-second cadence if a bucket boundary fell in
/// the half-open window `(after, now]` — i.e. `now` crossed into a new `span_secs`-wide
/// bucket since `after`. The boundary (bucket start) is the deterministic fire instant
/// (so the single-fire lock key is stable across pods). `None` if no boundary was
/// crossed (or `span_secs < 1`). Shared by the `GraphQuery` (minutes) and collect
/// (seconds) scanners so both bucket identically at their own resolution.
fn due_bucket(span_secs: i64, after: DateTime<Utc>, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    if span_secs < 1 {
        return None;
    }
    let bucket = |t: DateTime<Utc>| t.timestamp().div_euclid(span_secs);
    let n = bucket(now);
    (n > bucket(after))
        .then(|| DateTime::from_timestamp(n * span_secs, 0))
        .flatten()
}

/// The poll fire-instant for an `every_minutes` (`GraphQuery`) cadence — the
/// minute-resolution wrapper over [`due_bucket`]. `every_minutes` is validated to
/// `1..=MAX_GRAPH_EVERY_MINUTES` upstream, so `*60` is in range; `saturating_mul`
/// keeps a stray value from overflow-panicking (it just never fires).
fn graph_due(
    every_minutes: i64,
    after: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    due_bucket(every_minutes.saturating_mul(60), after, now)
}

/// The `(cron, fire-instant)` of the first `Schedule` trigger on `automation` that
/// is due in `(after, now]`, if any. Non-schedule / unparseable triggers are
/// ignored; an invalid cron/timezone is logged and skipped (it never silently
/// fires).
fn first_due(
    automation: &Automation,
    after: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Option<(String, DateTime<Utc>)> {
    automation.triggers.iter().find_map(|t| {
        match serde_json::from_value::<Trigger>(t.clone()) {
            Ok(Trigger::Schedule { cron, tz }) => match due_occurrence(&cron, tz.as_deref(), after, now) {
                Ok(Some(fire)) => Some((cron, fire)),
                Ok(None) => None,
                Err(e) => {
                    warn!(automation = %automation.id, error = %e, "skipping schedule with invalid cron/timezone");
                    None
                }
            },
            _ => None,
        }
    })
}

/// The clock scheduler (SOUL §11): a tokio loop that, every [`tick`](Self::with_tick),
/// scans for `Schedule` automations due since the last tick and enqueues their
/// runs, single-firing each occurrence via the bus's distributed lock. See the
/// module docs for the no-catch-up semantics.
pub struct ScheduleWorker {
    store: Store,
    bus: Bus,
    tick: Duration,
    last_tick: DateTime<Utc>,
    graph: Option<GraphStore>,
}

impl ScheduleWorker {
    /// A scheduler over `store` (single-firing via `bus`'s lock) with the default
    /// 1-minute tick, anchored at the current instant (no catch-up for past-due
    /// crons).
    #[must_use]
    pub fn new(store: Store, bus: Bus) -> Self {
        Self {
            store,
            bus,
            tick: DEFAULT_TICK,
            last_tick: Utc::now(),
            graph: None,
        }
    }

    /// Override the tick interval (tests use a short tick; cron granularity is
    /// still a minute, so a sub-minute tick just polls more often).
    #[must_use]
    pub fn with_tick(mut self, tick: Duration) -> Self {
        self.tick = tick;
        self
    }

    /// Attach a [`GraphStore`] so the worker also polls `GraphQuery` automations
    /// (SOUL §11/§6.3). Without it, `GraphQuery` triggers are inert (no graph to
    /// query) — the binary attaches it only when `[neo4j]` is enabled.
    #[must_use]
    pub fn with_graph(mut self, graph: GraphStore) -> Self {
        self.graph = Some(graph);
        self
    }

    /// Spawn the [`run`](Self::run) loop as a detached background task.
    #[must_use]
    pub fn spawn(self) -> JoinHandle<()> {
        tokio::spawn(self.run())
    }

    /// Tick forever: sleep, then scan `(last_tick, now]` and advance `last_tick`.
    /// A scan error is logged and retried next tick; the loop never exits.
    pub async fn run(mut self) {
        info!(tick_secs = self.tick.as_secs(), "schedule worker started");
        loop {
            tokio::time::sleep(self.tick).await;
            let now = Utc::now();
            match scan_schedules(&self.store, self.bus.lock(), self.last_tick, now).await {
                Ok(jobs) if !jobs.is_empty() => {
                    info!(count = jobs.len(), "enqueued scheduled automations")
                }
                Ok(_) => {}
                Err(e) => warn!(error = %e, "schedule scan failed; will retry next tick"),
            }
            // Time-driven calendar reminders share this clock loop + window.
            match scan_calendar_event_triggers(&self.store, self.bus.lock(), self.last_tick, now)
                .await
            {
                Ok(jobs) if !jobs.is_empty() => {
                    info!(count = jobs.len(), "enqueued calendar-event automations")
                }
                Ok(_) => {}
                Err(e) => warn!(error = %e, "calendar-event scan failed; will retry next tick"),
            }
            // Periodic graph-condition polls (only when a Neo4j is attached).
            if let Some(graph) = &self.graph {
                match scan_graph_queries(&self.store, graph, self.bus.lock(), self.last_tick, now)
                    .await
                {
                    Ok(jobs) if !jobs.is_empty() => {
                        info!(count = jobs.len(), "enqueued graph-query automations")
                    }
                    Ok(_) => {}
                    Err(e) => warn!(error = %e, "graph-query scan failed; will retry next tick"),
                }
            }
            // Email/calendar collect-source polls (SOUL §10/§28): enqueue a collect
            // job per due CollectEmail/CollectCalendar trigger; the sync worker does
            // the heavy provider pull + per-item fan-out off this clock.
            match scan_collect_triggers(&self.store, self.bus.lock(), self.last_tick, now).await {
                Ok(jobs) if !jobs.is_empty() => {
                    info!(count = jobs.len(), "enqueued collect jobs")
                }
                Ok(_) => {}
                Err(e) => warn!(error = %e, "collect scan failed; will retry next tick"),
            }
            self.last_tick = now;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        due_bucket, graph_due, parse_collect_every_secs, parse_duration_secs, parse_every_minutes,
        parse_lead_minutes, DEFAULT_COLLECT_EVERY_SECS, DEFAULT_GRAPH_EVERY_MINUTES,
        MAX_COLLECT_EVERY_SECS, MAX_GRAPH_EVERY_MINUTES, MIN_COLLECT_EVERY_SECS,
    };
    use chrono::DateTime;
    use serde_json::json;

    fn ts(secs: i64) -> DateTime<chrono::Utc> {
        DateTime::from_timestamp(secs, 0).unwrap()
    }

    #[test]
    fn every_accepts_number_object_default_and_rejects_out_of_range() {
        assert_eq!(parse_every_minutes(Some(&json!(10))), Some(10));
        assert_eq!(
            parse_every_minutes(Some(&json!({ "minutes": 15 }))),
            Some(15)
        );
        // Absent / null → the default cadence.
        assert_eq!(parse_every_minutes(None), Some(DEFAULT_GRAPH_EVERY_MINUTES));
        assert_eq!(
            parse_every_minutes(Some(&json!(null))),
            Some(DEFAULT_GRAPH_EVERY_MINUTES)
        );
        // Cadences up to a year are accepted (the cap is now 1 year); one minute
        // beyond it is rejected.
        assert_eq!(
            parse_every_minutes(Some(&json!(MAX_GRAPH_EVERY_MINUTES))),
            Some(MAX_GRAPH_EVERY_MINUTES)
        );
        assert_eq!(
            parse_every_minutes(Some(&json!(MAX_GRAPH_EVERY_MINUTES + 1))),
            None
        );
        // Out of range (≤0 or beyond the cap) and garbage shapes → skip.
        assert_eq!(parse_every_minutes(Some(&json!(0))), None);
        assert_eq!(parse_every_minutes(Some(&json!(-1))), None);
        assert_eq!(parse_every_minutes(Some(&json!(i64::MAX))), None);
        assert_eq!(parse_every_minutes(Some(&json!("5m"))), None);
    }

    #[test]
    fn graph_due_fires_once_per_bucket_boundary_crossed() {
        // every = 5m → 300s buckets. Crossing 0→1 fires at the boundary (ts 300).
        assert_eq!(graph_due(5, ts(0), ts(301)), Some(ts(300)));
        // Same bucket on both ends → no boundary crossed → None.
        assert_eq!(graph_due(5, ts(0), ts(299)), None);
        assert_eq!(graph_due(5, ts(301), ts(305)), None);
        // The fire instant is the bucket start, stable regardless of where in the
        // window `now` lands (so the single-fire lock key is deterministic).
        assert_eq!(graph_due(5, ts(250), ts(330)), Some(ts(300)));
        // A sub-1 interval is rejected (never divides by zero / fires).
        assert_eq!(graph_due(0, ts(0), ts(10_000)), None);
    }

    #[test]
    fn due_bucket_buckets_at_second_resolution() {
        // Collect uses second-resolution buckets (graph_due is the *60 wrapper). A
        // 60s cadence fires once when `now` crosses into a new 60s bucket.
        assert_eq!(due_bucket(60, ts(0), ts(61)), Some(ts(60)));
        assert_eq!(due_bucket(60, ts(0), ts(59)), None);
        // A 30s cadence resolves sub-minute (the wrapper couldn't express this).
        assert_eq!(due_bucket(30, ts(0), ts(31)), Some(ts(30)));
        // The boundary is the bucket start regardless of where `now` lands.
        assert_eq!(due_bucket(90, ts(10), ts(200)), Some(ts(180)));
        // span < 1 never fires (no divide-by-zero).
        assert_eq!(due_bucket(0, ts(0), ts(10_000)), None);
    }

    #[test]
    fn collect_every_reads_number_object_string_default_and_clamps() {
        // Absent / null → the shared-tick default (SOUL §29: shared tick as default).
        assert_eq!(
            parse_collect_every_secs(None),
            Some(DEFAULT_COLLECT_EVERY_SECS)
        );
        assert_eq!(
            parse_collect_every_secs(Some(&json!(null))),
            Some(DEFAULT_COLLECT_EVERY_SECS)
        );
        // A bare number is MINUTES (the shared `every` convention) → seconds.
        assert_eq!(parse_collect_every_secs(Some(&json!(5))), Some(5 * 60));
        // Object forms: minutes and (sub-minute) seconds.
        assert_eq!(
            parse_collect_every_secs(Some(&json!({ "minutes": 15 }))),
            Some(15 * 60)
        );
        assert_eq!(
            parse_collect_every_secs(Some(&json!({ "seconds": 90 }))),
            Some(90)
        );
        // Duration strings.
        assert_eq!(parse_collect_every_secs(Some(&json!("5m"))), Some(300));
        assert_eq!(parse_collect_every_secs(Some(&json!("1h"))), Some(3_600));
        // Too-small clamps UP to the floor rather than erroring (SOUL §29).
        assert_eq!(
            parse_collect_every_secs(Some(&json!("30s"))),
            Some(MIN_COLLECT_EVERY_SECS)
        );
        assert_eq!(
            parse_collect_every_secs(Some(&json!(0))),
            Some(MIN_COLLECT_EVERY_SECS)
        );
        assert_eq!(
            parse_collect_every_secs(Some(&json!(-5))),
            Some(MIN_COLLECT_EVERY_SECS)
        );
        // Cadences up to a year are honored (the ceiling is now 1 year): a year of
        // minutes / a `1y` string land exactly on the ceiling, not clamped below it.
        assert_eq!(
            parse_collect_every_secs(Some(&json!({ "minutes": 525_600 }))),
            Some(MAX_COLLECT_EVERY_SECS)
        );
        assert_eq!(
            parse_collect_every_secs(Some(&json!("1y"))),
            Some(MAX_COLLECT_EVERY_SECS)
        );
        // Beyond a year clamps DOWN to the ceiling rather than erroring.
        assert_eq!(
            parse_collect_every_secs(Some(&json!({ "minutes": 10_000_000 }))),
            Some(MAX_COLLECT_EVERY_SECS)
        );
        assert_eq!(
            parse_collect_every_secs(Some(&json!(i64::MAX))),
            Some(MAX_COLLECT_EVERY_SECS)
        );
        // Genuinely uninterpretable shapes → None (the trigger is skipped + warned).
        assert_eq!(parse_collect_every_secs(Some(&json!(1.5))), None);
        assert_eq!(parse_collect_every_secs(Some(&json!("soon"))), None);
        assert_eq!(parse_collect_every_secs(Some(&json!({ "days": 1 }))), None);
        assert_eq!(parse_collect_every_secs(Some(&json!([1, 2]))), None);
    }

    #[test]
    fn first_collect_trigger_skips_placeholder_connection_ids() {
        use super::first_collect_trigger;
        use catalerum_core::{Automation, AutomationId, ConnectionId, WorkspaceId};
        let automation = |connection: &str| Automation {
            id: AutomationId::new(),
            workspace_id: WorkspaceId::new(),
            name: "collect".into(),
            enabled: true,
            triggers: vec![json!({ "kind": "collect_email", "connection": connection })],
            condition: None,
            actions: vec![json!({ "kind": "write_email" })],
            spec: None,
            grant_id: None,
        };
        // A placeholder name never enqueues — the job could only ever fail in the
        // worker, spamming a doomed retry every tick.
        assert!(first_collect_trigger(&automation("fastmail")).is_none());
        // A real connection id is picked up, at the default cadence.
        let id = ConnectionId::new().to_string();
        let (trigger, secs) = first_collect_trigger(&automation(&id)).unwrap();
        assert_eq!(trigger.collect_connection(), Some(id.as_str()));
        assert_eq!(secs, DEFAULT_COLLECT_EVERY_SECS);
        // A padded id still parses (trimmed like the worker's parse_connection).
        assert!(first_collect_trigger(&automation(&format!("  {id}  "))).is_some());
    }

    #[test]
    fn duration_secs_parses_units_and_rejects_garbage() {
        assert_eq!(parse_duration_secs("30s"), Some(30));
        assert_eq!(parse_duration_secs("5m"), Some(300));
        assert_eq!(parse_duration_secs("1h"), Some(3_600));
        assert_eq!(parse_duration_secs("2d"), Some(172_800));
        // Weeks and years — `1y` (365 days) is the advertised max cadence; the web
        // editor accepts both units, so the server must interpret them rather than
        // skip the trigger.
        assert_eq!(parse_duration_secs("1w"), Some(604_800));
        assert_eq!(parse_duration_secs("1W"), Some(604_800));
        assert_eq!(parse_duration_secs("1y"), Some(31_536_000));
        assert_eq!(parse_duration_secs("1Y"), Some(31_536_000));
        // Compound + case-insensitive + whitespace-tolerant.
        assert_eq!(parse_duration_secs("1h30m"), Some(5_400));
        assert_eq!(parse_duration_secs("  1H30M  "), Some(5_400));
        // Rejected: unit-less (would collide with the number=minutes reading), empty,
        // stray/unknown units, a unit with no number, a trailing bare number.
        assert_eq!(parse_duration_secs("300"), None);
        assert_eq!(parse_duration_secs(""), None);
        assert_eq!(parse_duration_secs("5x"), None);
        assert_eq!(parse_duration_secs("m"), None);
        assert_eq!(parse_duration_secs("5m30"), None);
    }

    #[test]
    fn lead_accepts_number_object_and_absent_rejects_garbage() {
        // A bare number = minutes.
        assert_eq!(parse_lead_minutes(Some(&json!(10))), Some(10));
        assert_eq!(parse_lead_minutes(Some(&json!(0))), Some(0));
        // Negative = fire after the start (allowed; it's just arithmetic).
        assert_eq!(parse_lead_minutes(Some(&json!(-5))), Some(-5));
        // `{ "minutes": N }` object form.
        assert_eq!(
            parse_lead_minutes(Some(&json!({ "minutes": 15 }))),
            Some(15)
        );
        // Absent / null → fire at start.
        assert_eq!(parse_lead_minutes(None), Some(0));
        assert_eq!(parse_lead_minutes(Some(&json!(null))), Some(0));
        // Uninterpretable shapes → None (the trigger is skipped, never mis-fires).
        assert_eq!(parse_lead_minutes(Some(&json!("10m"))), None);
        assert_eq!(parse_lead_minutes(Some(&json!({ "hours": 1 }))), None);
        assert_eq!(parse_lead_minutes(Some(&json!(1.5))), None);
        // An absurd / overflowing lead is capped out (skipped), never panicking the
        // scheduler on the downstream Duration/datetime arithmetic.
        assert_eq!(
            parse_lead_minutes(Some(&json!(super::MAX_LEAD_MINUTES))),
            Some(super::MAX_LEAD_MINUTES)
        );
        assert_eq!(
            parse_lead_minutes(Some(&json!(super::MAX_LEAD_MINUTES + 1))),
            None
        );
        assert_eq!(parse_lead_minutes(Some(&json!(i64::MAX))), None);
        assert_eq!(parse_lead_minutes(Some(&json!(i64::MIN))), None);
    }
}
