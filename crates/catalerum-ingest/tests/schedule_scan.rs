//! Integration test: the clock scheduler scan (SOUL §11). `scan_schedules`
//! enqueues a `run_automation` job for each enabled `Schedule` automation whose
//! cron fired in the window, and ignores disabled ones and non-schedule triggers.
//!
//! DB-gated like the other ingest tests: set `CATALERUM_TEST_DATABASE_URL` (or
//! `DATABASE_URL`) to run it; otherwise it skips and passes offline.

mod common;

use catalerum_bus::{Bus, InProcessLock};
use catalerum_core::id::{AutomationId, WorkspaceId};
use catalerum_core::model::ConnectionKind;
use catalerum_core::{Author, Note, NoteId, UserId};
use catalerum_graph::GraphStore;
use catalerum_ingest::{
    enqueue_collect_now, scan_calendar_event_triggers, scan_graph_queries, scan_schedules,
    CollectPayload, RunAutomationPayload,
};
use catalerum_store::{NewAutomation, Store, UpsertEvent};
use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};
use std::collections::HashSet;

fn db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

fn automation(name: &str, enabled: bool, triggers: Vec<Value>) -> NewAutomation {
    NewAutomation {
        name: name.to_string(),
        enabled,
        triggers,
        condition: None,
        actions: vec![json!({ "kind": "summarize" })],
        spec: None,
        grant_id: None,
    }
}

/// The automation ids referenced by the `run_automation` jobs `scan_schedules`
/// returned — looked up from the job rows, so the assertion is robust to other
/// tests' jobs sharing the queue.
async fn enqueued_automation_ids(store: &Store, jobs: &[uuid::Uuid]) -> HashSet<AutomationId> {
    let mut ids = HashSet::new();
    for job_id in jobs {
        let row = store.job_queue().get(*job_id).await.expect("get job");
        let payload: RunAutomationPayload =
            serde_json::from_value(row.payload().clone()).expect("payload");
        ids.insert(payload.automation_id);
    }
    ids
}

#[tokio::test]
async fn scan_enqueues_due_schedule_automations_only() {
    let Some(url) = db_url() else {
        eprintln!("skipping schedule scan test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    let store = common::isolated_store(&url).await;
    let ws = store
        .workspaces()
        .create("sched", &format!("sched-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");

    // Enabled, every-minute schedule → due in a 2-minute window.
    let due = store
        .automations()
        .create(
            ws.id,
            &automation(
                "nightly",
                true,
                vec![json!({ "kind": "schedule", "cron": "* * * * *" })],
            ),
        )
        .await
        .unwrap();
    // Disabled schedule → never enqueued.
    let disabled = store
        .automations()
        .create(
            ws.id,
            &automation(
                "paused",
                false,
                vec![json!({ "kind": "schedule", "cron": "* * * * *" })],
            ),
        )
        .await
        .unwrap();
    // Enabled but a non-schedule (webhook) trigger → not the scheduler's job.
    let webhook = store
        .automations()
        .create(
            ws.id,
            &automation(
                "on-hook",
                true,
                vec![json!({ "kind": "webhook", "path": "/x" })],
            ),
        )
        .await
        .unwrap();
    // Enabled schedule whose cron does NOT fire in the window (9am, window is "now-ish").
    let nine_am = store
        .automations()
        .create(
            ws.id,
            &automation(
                "morning",
                true,
                vec![json!({ "kind": "schedule", "cron": "0 9 * * *" })],
            ),
        )
        .await
        .unwrap();

    let lock = InProcessLock::new();
    let now = Utc::now();
    let after = now - Duration::minutes(2);
    let jobs = scan_schedules(&store, &lock, after, now)
        .await
        .expect("scan");
    let ids = enqueued_automation_ids(&store, &jobs).await;

    assert!(
        ids.contains(&due.id),
        "the enabled every-minute schedule was enqueued"
    );
    assert!(
        !ids.contains(&disabled.id),
        "a disabled automation is never scheduled"
    );
    assert!(
        !ids.contains(&webhook.id),
        "a non-schedule trigger is not the scheduler's job"
    );
    // The 9am automation only fires if `now` happens to be in its 09:00 minute —
    // assert it's excluded except in that rare window, to keep the test deterministic.
    let nine_am_due = catalerum_automation::due_in_window("0 9 * * *", None, after, now).unwrap();
    assert_eq!(ids.contains(&nine_am.id), nine_am_due);

    // A zero-width window (after == now) is never due → nothing enqueued for it.
    let none = scan_schedules(&store, &lock, now, now)
        .await
        .expect("scan2");
    let none_ids = enqueued_automation_ids(&store, &none).await;
    assert!(
        !none_ids.contains(&due.id),
        "a zero-width window double-fires nothing"
    );
}

/// Single-fire (SOUL §11/§6.2): two scans of the **same window** sharing one lock
/// — modelling two pods racing on the same pending occurrence — enqueue the run
/// **exactly once**. The second scan's occurrence claim is already held, so it
/// skips. (A fresh lock would let it fire again, proving the lock is the gate.)
#[tokio::test]
async fn concurrent_scans_single_fire_one_occurrence() {
    let Some(url) = db_url() else {
        eprintln!("skipping single-fire test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    let store = common::isolated_store(&url).await;
    let ws = store
        .workspaces()
        .create("single", &format!("single-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let auto = store
        .automations()
        .create(
            ws.id,
            &automation(
                "minutely",
                true,
                vec![json!({ "kind": "schedule", "cron": "* * * * *" })],
            ),
        )
        .await
        .unwrap();

    let now = Utc::now();
    let after = now - Duration::minutes(2);
    // A shared lock = the two "pods" coordinate; the same occurrence fires once.
    let shared = Bus::in_process();
    let first = scan_schedules(&store, shared.lock(), after, now)
        .await
        .expect("scan A");
    let second = scan_schedules(&store, shared.lock(), after, now)
        .await
        .expect("scan B");
    let fired_once = enqueued_automation_ids(&store, &first)
        .await
        .contains(&auto.id);
    let fired_twice = enqueued_automation_ids(&store, &second)
        .await
        .contains(&auto.id);
    assert!(
        fired_once,
        "the first scan claimed + enqueued the occurrence"
    );
    assert!(
        !fired_twice,
        "the second scan saw the occurrence already claimed → no double-fire"
    );

    // Control: an INDEPENDENT lock has no record of the claim, so it fires again —
    // proving it's the shared lock (not some other dedup) that single-fires.
    let fresh = Bus::in_process();
    let third = scan_schedules(&store, fresh.lock(), after, now)
        .await
        .expect("scan C");
    assert!(
        enqueued_automation_ids(&store, &third)
            .await
            .contains(&auto.id),
        "a fresh lock re-fires the same occurrence (the lock is the single-fire gate)"
    );
}

/// The same single-fire guarantee over a **real Valkey** `RedisLock` — the actual
/// multi-pod HA path (SOUL §6.2/§11). Two separate `Bus::connect` handles model two
/// pods sharing Valkey; the occurrence fires once. Gated on `CATALERUM_TEST_VALKEY_URL`.
#[tokio::test]
async fn concurrent_scans_single_fire_over_valkey() {
    let (Some(url), Ok(valkey)) = (db_url(), std::env::var("CATALERUM_TEST_VALKEY_URL")) else {
        eprintln!("skipping valkey single-fire test: set CATALERUM_TEST_DATABASE_URL + CATALERUM_TEST_VALKEY_URL");
        return;
    };
    let store = common::isolated_store(&url).await;
    let ws = store
        .workspaces()
        .create("vsingle", &format!("vsingle-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let auto = store
        .automations()
        .create(
            ws.id,
            &automation(
                "vminutely",
                true,
                vec![json!({ "kind": "schedule", "cron": "* * * * *" })],
            ),
        )
        .await
        .unwrap();

    // Two independent connections to the same Valkey = two pods.
    let pod_a = Bus::connect(&valkey).await.expect("valkey A");
    let pod_b = Bus::connect(&valkey).await.expect("valkey B");
    let now = Utc::now();
    let after = now - Duration::minutes(2);
    let a = scan_schedules(&store, pod_a.lock(), after, now)
        .await
        .expect("pod A");
    let b = scan_schedules(&store, pod_b.lock(), after, now)
        .await
        .expect("pod B");
    let a_fired = enqueued_automation_ids(&store, &a).await.contains(&auto.id);
    let b_fired = enqueued_automation_ids(&store, &b).await.contains(&auto.id);
    assert!(
        a_fired ^ b_fired,
        "exactly one pod fired the occurrence over Valkey (got A={a_fired} B={b_fired})"
    );
}

/// Seed a calendar event starting at `start` with `summary` (its own connection +
/// calendar so names never collide), for the `CalendarEvent`-trigger scan tests
/// below.
async fn seed_event(
    store: &Store,
    ws: WorkspaceId,
    uid: &str,
    start: DateTime<Utc>,
    summary: &str,
) {
    let conn = store
        .connections()
        .create(
            ws,
            ConnectionKind::Calendar,
            &format!("cal-{uid}"),
            None,
            None,
        )
        .await
        .expect("calendar connection");
    let cal = store
        .calendars()
        .upsert(ws, conn.id, &format!("ext-{uid}"), "Work", false)
        .await
        .expect("calendar");
    store
        .events()
        .upsert_by_uid(&UpsertEvent {
            workspace_id: ws,
            calendar_id: cal.id,
            uid,
            starts_at: start,
            ends_at: start + Duration::hours(1),
            all_day: false,
            rrule: None,
            summary,
            location: None,
            body: None,
            attendees: &[],
            labels: &[],
            attachments: &[],
            etag: None,
            sequence: 0,
        })
        .await
        .expect("event");
}

/// `CalendarEvent`-lead trigger source (SOUL §11/§8): an enabled `calendar_event`
/// automation fires once for each event whose lead instant (`start − lead`) crosses
/// the tick window; a not-yet-due event, a disabled automation, and a non-calendar
/// trigger are all excluded; and a re-scan of the same window sharing one lock does
/// not double-fire.
#[tokio::test]
async fn scan_enqueues_due_calendar_event_reminders_single_fire() {
    let Some(url) = db_url() else {
        eprintln!(
            "skipping calendar-event scan test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
        );
        return;
    };
    let store = common::isolated_store(&url).await;
    let ws = store
        .workspaces()
        .create("calev", &format!("calev-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");

    let now = Utc::now();
    // Lead = 10m. A `start = now+9m` event has lead instant `now−1m` ∈ (now−2m, now]
    // → DUE this tick. A `start = now+60m` event's lead instant is 50m out → NOT due.
    seed_event(&store, ws.id, "soon", now + Duration::minutes(9), "Standup").await;
    seed_event(
        &store,
        ws.id,
        "later",
        now + Duration::minutes(60),
        "Standup",
    )
    .await;

    let remind = store
        .automations()
        .create(
            ws.id,
            &automation(
                "remind",
                true,
                vec![json!({ "kind": "calendar_event", "lead": 10 })],
            ),
        )
        .await
        .unwrap();
    let paused = store
        .automations()
        .create(
            ws.id,
            &automation(
                "paused",
                false,
                vec![json!({ "kind": "calendar_event", "lead": 10 })],
            ),
        )
        .await
        .unwrap();
    let sched = store
        .automations()
        .create(
            ws.id,
            &automation(
                "sched",
                true,
                vec![json!({ "kind": "schedule", "cron": "* * * * *" })],
            ),
        )
        .await
        .unwrap();

    let lock = InProcessLock::new();
    let after = now - Duration::minutes(2);
    let jobs = scan_calendar_event_triggers(&store, &lock, after, now)
        .await
        .expect("scan");
    let ids = enqueued_automation_ids(&store, &jobs).await;

    assert!(ids.contains(&remind.id), "the due event fired its reminder");
    assert!(
        !ids.contains(&paused.id),
        "a disabled automation never fires"
    );
    assert!(
        !ids.contains(&sched.id),
        "a schedule trigger isn't the calendar scan's job"
    );
    assert_eq!(
        jobs.len(),
        1,
        "only the due event fired (the +60m event is not yet due), exactly once"
    );

    // The recorded trigger references the firing event (audit + LlmAgent seed).
    let job = store.job_queue().get(jobs[0]).await.expect("job");
    let trigger = job.payload()["trigger"].clone();
    assert_eq!(trigger["kind"], "calendar_event");
    assert_eq!(trigger["lead_minutes"], 10);
    assert!(
        trigger["event_id"].is_string(),
        "the firing event id is recorded"
    );

    // Single-fire: re-scan the SAME window with the SAME lock → the (automation,
    // event) claim is held → no double-fire.
    let again = scan_calendar_event_triggers(&store, &lock, after, now)
        .await
        .expect("scan2");
    assert!(
        !enqueued_automation_ids(&store, &again)
            .await
            .contains(&remind.id),
        "the same (automation, event) claim is held → no double-fire"
    );
}

/// `CalendarEvent` trigger `filter` (SOUL §8/§11): a `{"summary": …}` predicate
/// gates which events fire. Two events are due in the same window — one whose
/// summary contains the filter substring (case-insensitive), one that doesn't —
/// and only the matching event enqueues a reminder.
#[tokio::test]
async fn calendar_event_filter_gates_which_events_fire() {
    let Some(url) = db_url() else {
        eprintln!(
            "skipping calendar-event filter test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
        );
        return;
    };
    let store = common::isolated_store(&url).await;
    let ws = store
        .workspaces()
        .create("calfilt", &format!("calfilt-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");

    let now = Utc::now();
    // Both events are due this tick (lead 10m, start now+9m → lead instant now−1m).
    // Only "Daily standup" contains the filter's "standup"; "Lunch break" does not.
    seed_event(
        &store,
        ws.id,
        "match",
        now + Duration::minutes(9),
        "Daily STANDUP",
    )
    .await;
    seed_event(
        &store,
        ws.id,
        "miss",
        now + Duration::minutes(9),
        "Lunch break",
    )
    .await;

    let remind = store
        .automations()
        .create(
            ws.id,
            &automation(
                "remind-standup",
                true,
                vec![json!({
                    "kind": "calendar_event", "lead": 10,
                    "filter": { "summary": "standup" }
                })],
            ),
        )
        .await
        .unwrap();

    let lock = InProcessLock::new();
    let after = now - Duration::minutes(2);
    let jobs = scan_calendar_event_triggers(&store, &lock, after, now)
        .await
        .expect("scan");
    let ids = enqueued_automation_ids(&store, &jobs).await;

    assert!(
        ids.contains(&remind.id),
        "the event whose summary matches the filter fired"
    );
    assert_eq!(
        jobs.len(),
        1,
        "only the matching event fired — the filter excluded 'Lunch break'"
    );
    // The recorded trigger references the matching event, not the excluded one.
    let job = store.job_queue().get(jobs[0]).await.expect("job");
    assert_eq!(job.payload()["trigger"]["summary"], "Daily STANDUP");
}

/// "Collect now" (SOUL §29): a manual one-shot poll of a Collect-headed automation
/// enqueues **exactly one immediate collect job** — the same durable job the scheduler
/// enqueues on the trigger's `every` cadence, only right now (bypassing the cadence
/// bucket). The job carries the automation + its collect trigger, ready for the sync
/// worker. A non-collect automation is not collectable → `None` (no job).
#[tokio::test]
async fn collect_now_enqueues_one_immediate_collect_job() {
    let Some(url) = db_url() else {
        eprintln!("skipping collect-now test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    let store = common::isolated_store(&url).await;
    let ws = store
        .workspaces()
        .create("cnow", &format!("cnow-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");

    // A collect automation. Its `every` is irrelevant here — collect-now bypasses the
    // cadence — but a duration string exercises the new §29 cadence shape end to end.
    let collect = store
        .automations()
        .create(
            ws.id,
            &automation(
                "collect-now",
                true,
                vec![json!({ "kind": "collect_email", "connection": "conn-1", "every": "1h" })],
            ),
        )
        .await
        .unwrap();
    // A non-collect automation → not collectable.
    let sched = store
        .automations()
        .create(
            ws.id,
            &automation(
                "sched",
                true,
                vec![json!({ "kind": "schedule", "cron": "* * * * *" })],
            ),
        )
        .await
        .unwrap();

    // Collect-now on the collect automation enqueues exactly one collect job that
    // carries this automation + its collect trigger.
    let job = enqueue_collect_now(&store, ws.id, &collect)
        .await
        .expect("collect now")
        .expect("a collect job id for a collect automation");
    let row = store.job_queue().get(job).await.expect("job row");
    let payload: CollectPayload =
        serde_json::from_value(row.payload().clone()).expect("collect payload");
    assert_eq!(
        payload.automation_id, collect.id,
        "the job targets this automation"
    );
    assert_eq!(
        payload.trigger["kind"], "collect_email",
        "the enqueued job is a collect poll (not a run_automation)"
    );

    // A non-collect automation yields None — nothing to collect, so the caller can
    // surface a 400 rather than enqueuing a meaningless job.
    assert!(
        enqueue_collect_now(&store, ws.id, &sched)
            .await
            .expect("no-op")
            .is_none(),
        "a non-collect automation is not collectable"
    );
}

/// A Neo4j handle for the `GraphQuery` scan test, from `NEO4J_URL`
/// (+ optional `NEO4J_USER`/`NEO4J_PASSWORD`, defaulting to `neo4j`/`catalerum`).
/// `None` (env unset) → the test skips.
fn graph_store() -> Option<GraphStore> {
    let url = std::env::var("NEO4J_URL").ok()?;
    let user = std::env::var("NEO4J_USER").unwrap_or_else(|_| "neo4j".into());
    let password = std::env::var("NEO4J_PASSWORD").unwrap_or_else(|_| "catalerum".into());
    Some(
        GraphStore::new(&url)
            .expect("valid NEO4J_URL")
            .with_auth(user, password),
    )
}

/// A minimal `:Note` to seed into a workspace so a Datalog `?- note(N).` goal has a
/// row to match (an empty graph yields no rows for any goal).
fn seed_note(ws: WorkspaceId) -> Note {
    Note {
        id: NoteId::new(),
        workspace_id: ws,
        author: Author::User { id: UserId::new() },
        title: "seed".into(),
        markdown: String::new(),
        tags: vec![],
        updated_at: Utc::now(),
    }
}

/// `GraphQuery`-poll trigger source (SOUL §11/§6.3): an enabled `graph_query`
/// automation whose Datalog goal returns rows fires when its `every` bucket boundary
/// is crossed; one whose goal returns nothing is polled but never fires; a disabled
/// automation is excluded; and a re-scan of the same window sharing one lock does
/// not double-fire. Requires Postgres + Neo4j (`NEO4J_URL`); skips otherwise.
#[tokio::test]
async fn scan_graph_queries_fires_on_nonempty_result_single_fire() {
    let (Some(url), Some(graph)) = (db_url(), graph_store()) else {
        eprintln!("skipping graph-query scan test: set CATALERUM_TEST_DATABASE_URL/DATABASE_URL and NEO4J_URL");
        return;
    };
    let store = common::isolated_store(&url).await;
    let ws = store
        .workspaces()
        .create("gq", &format!("gq-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");

    // Seed one :Note into this workspace so `?- note(N).` has a row to match.
    graph.delete_workspace(ws.id).await.expect("clean slate");
    graph
        .project_note(&seed_note(ws.id), &[])
        .await
        .expect("seed note");

    // A Datalog goal that matches the seeded note → fires.
    let firing = store
        .automations()
        .create(
            ws.id,
            &automation(
                "watch",
                true,
                vec![json!({ "kind": "graph_query", "query": "?- note(N).", "every": 1 })],
            ),
        )
        .await
        .unwrap();
    // A goal that matches nothing (no topics were projected) → polled, never fires.
    let quiet = store
        .automations()
        .create(
            ws.id,
            &automation(
                "quiet",
                true,
                vec![json!({ "kind": "graph_query", "query": "?- topic(T).", "every": 1 })],
            ),
        )
        .await
        .unwrap();
    // Disabled → never fires even though its goal would match.
    let disabled = store
        .automations()
        .create(
            ws.id,
            &automation(
                "off",
                false,
                vec![json!({ "kind": "graph_query", "query": "?- note(N).", "every": 1 })],
            ),
        )
        .await
        .unwrap();

    let lock = InProcessLock::new();
    // every = 1m (60s buckets); a 2-minute window always crosses a boundary.
    let now = Utc::now();
    let after = now - Duration::minutes(2);
    let jobs = scan_graph_queries(&store, &graph, &lock, after, now)
        .await
        .expect("scan");
    let ids = enqueued_automation_ids(&store, &jobs).await;

    assert!(ids.contains(&firing.id), "a goal returning rows fires");
    assert!(
        !ids.contains(&quiet.id),
        "a goal returning nothing does not fire"
    );
    assert!(
        !ids.contains(&disabled.id),
        "a disabled automation never fires"
    );
    assert_eq!(
        jobs.len(),
        1,
        "only the matching, enabled graph_query fired"
    );

    // The recorded trigger carries the query + row count (audit + LlmAgent seed).
    let job = store.job_queue().get(jobs[0]).await.expect("job");
    let trigger = job.payload()["trigger"].clone();
    assert_eq!(trigger["kind"], "graph_query");
    assert!(
        trigger["rows"].as_u64().unwrap_or(0) >= 1,
        "the row count is recorded"
    );

    // Single-fire: re-scan the same window + lock → the poll occurrence is claimed.
    let again = scan_graph_queries(&store, &graph, &lock, after, now)
        .await
        .expect("scan2");
    assert!(
        !enqueued_automation_ids(&store, &again)
            .await
            .contains(&firing.id),
        "the same poll occurrence is claimed → no double-fire"
    );
}
