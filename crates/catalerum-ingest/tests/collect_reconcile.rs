//! Integration tests for the hardened collect pipeline (SOUL §11/§19/§28):
//!
//! 1. **Capability enforcement** — a collect poll runs only if the automation's
//!    recorded §19 grant covers `email:read@<connection>` (the collect
//!    capability); a grant that omits the connection fails the run **closed**
//!    (`IngestError::Forbidden`), while a covering grant — or no grant at all
//!    (the base-Member `role_grant` fallback) — lets the poll proceed.
//! 2. **Upstream-deletion reconciliation** — a snapshot provider (Maildir) whose
//!    source loses a message has the stored email row hard-deleted on the next
//!    poll (and the derived-projection purge enqueued), idempotently.
//!
//! DB-gated like the other ingest tests: set `CATALERUM_TEST_DATABASE_URL` (or
//! `DATABASE_URL`) to run; otherwise they skip and pass offline.

mod common;

use std::fs;
use std::sync::Arc;

use catalerum_automation::{Action, ActionOutcome, ActionRunner};
use catalerum_core::capability::{Action as CapAction, Capability, Constraints, Resource};
use catalerum_core::model::{ConnectionKind, Email, EmailAddress};
use catalerum_core::{EmailId, WorkspaceId};
use catalerum_ingest::{
    run_collect_calendar, run_collect_email, AutomationContext, CollectPayload, IngestError,
};
use catalerum_store::{NewAutomation, Store, UpsertEvent};
use serde_json::{json, Value};

fn db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

/// A runner that succeeds every action — the per-item runs need only complete;
/// the real store-writing runner is tested in `catalerum-api`.
struct SuccessRunner;

#[async_trait::async_trait]
impl ActionRunner for SuccessRunner {
    async fn run(
        &self,
        _workspace_id: WorkspaceId,
        _action: &Action,
        _trigger: Option<&Value>,
        _grant: Option<&catalerum_core::model::Grant>,
    ) -> ActionOutcome {
        ActionOutcome::succeeded(None)
    }
}

fn collect_trigger(connection: &str) -> Value {
    json!({ "kind": "collect_email", "connection": connection })
}

fn collect_automation(name: &str, connection: &str) -> NewAutomation {
    NewAutomation {
        name: name.to_string(),
        enabled: true,
        triggers: vec![collect_trigger(connection)],
        condition: None,
        actions: vec![json!({ "kind": "summarize" })],
        spec: None,
        grant_id: None,
    }
}

/// The collect capability for one connection: `email:read@<connection>`.
fn email_collect_cap(connection: &str) -> Capability {
    Capability::new(CapAction::Read, Resource::new("email", connection))
}

/// The §19 gate on the collect poll: an automation whose grant omits the
/// trigger's connection fails **closed** (`Forbidden` — a clear job failure, no
/// silent skip); a covering grant, a domain-wide grant, or no grant at all (the
/// base-Member fallback authority) passes the gate.
#[tokio::test]
async fn collect_poll_enforces_the_grant_scoped_collect_capability() {
    let Some(url) = db_url() else {
        eprintln!(
            "skipping collect capability test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
        );
        return;
    };
    let store = common::isolated_store(&url).await;
    let ws = store
        .workspaces()
        .create(
            "collectcap",
            &format!("collectcap-{}", uuid::Uuid::new_v4()),
        )
        .await
        .expect("ws");
    let runner: Arc<dyn ActionRunner> = Arc::new(SuccessRunner);
    let ctx = AutomationContext::new(runner);

    // The connection the trigger names. No row exists for it: the deny case
    // fails before any store/provider access, and the allow cases prove the gate
    // opened by failing *later* (a store NotFound, never Forbidden).
    let conn = uuid::Uuid::new_v4().to_string();
    let other = uuid::Uuid::new_v4().to_string();

    let run = |automation: catalerum_core::Automation| {
        let store = store.clone();
        let ctx = ctx.clone();
        async move {
            let trigger = automation.triggers[0].clone();
            let payload = CollectPayload::new(ws.id, automation.id, trigger);
            run_collect_email(&store, &ctx, ws.id, &payload).await
        }
    };

    // (a) A grant scoped to a DIFFERENT connection → denied, fail-closed.
    let narrow = store
        .grants()
        .upsert(
            ws.id,
            "narrow",
            &[email_collect_cap(&other)],
            &Constraints::default(),
        )
        .await
        .expect("grant");
    let mut spec = collect_automation("denied", &conn);
    spec.grant_id = Some(narrow.id);
    let denied = store.automations().create(ws.id, &spec).await.unwrap();
    let err = run(denied)
        .await
        .expect_err("uncovered grant must fail the poll");
    assert!(
        matches!(&err, IngestError::Forbidden(_)),
        "expected Forbidden, got: {err}"
    );
    assert!(
        err.to_string().contains("collect denied"),
        "the error names the denial: {err}"
    );

    // (b) A grant covering exactly this connection → the gate opens (the poll
    // then fails on the missing connection row — a Store error, NOT Forbidden).
    let exact = store
        .grants()
        .upsert(
            ws.id,
            "exact",
            &[email_collect_cap(&conn)],
            &Constraints::default(),
        )
        .await
        .unwrap();
    let mut spec = collect_automation("covered", &conn);
    spec.grant_id = Some(exact.id);
    let covered = store.automations().create(ws.id, &spec).await.unwrap();
    let err = run(covered).await.expect_err("no connection row yet");
    assert!(
        !matches!(&err, IngestError::Forbidden(_)),
        "a covering grant must pass the capability gate, got: {err}"
    );

    // (c) A domain-wide `email:read` grant covers any connection selector.
    let wide = store
        .grants()
        .upsert(
            ws.id,
            "wide",
            &[Capability::new(CapAction::Read, Resource::domain("email"))],
            &Constraints::default(),
        )
        .await
        .unwrap();
    let mut spec = collect_automation("domainwide", &conn);
    spec.grant_id = Some(wide.id);
    let domainwide = store.automations().create(ws.id, &spec).await.unwrap();
    let err = run(domainwide).await.expect_err("no connection row yet");
    assert!(!matches!(&err, IngestError::Forbidden(_)));

    // (d) No grant at all → the run executes under the runner's default bounded
    // base-Member authority (the §19 role_grant fallback, which implies collect
    // for a Member) — the gate passes it through.
    let ungranted = store
        .automations()
        .create(ws.id, &collect_automation("ungranted", &conn))
        .await
        .unwrap();
    let err = run(ungranted).await.expect_err("no connection row yet");
    assert!(!matches!(&err, IngestError::Forbidden(_)));
}

/// Write a minimal RFC 5322 message into a Maildir's `new/`.
fn write_message(root: &std::path::Path, uid: &str, subject: &str) {
    fs::write(
        root.join("new").join(uid),
        format!("Subject: {subject}\r\nFrom: ada@example.com\r\n\r\nbody of {uid}\r\n"),
    )
    .expect("write maildir message");
}

/// A stored email row for `(mailbox, uid)` — what a downstream `WriteEmail`
/// would have persisted for a collected item.
fn stored_email(ws: WorkspaceId, mailbox_id: catalerum_core::MailboxId, uid: &str) -> Email {
    Email {
        id: EmailId::new(),
        workspace_id: ws,
        mailbox_id,
        uid: uid.to_string(),
        message_id: None,
        from: Some(EmailAddress::new("ada@example.com")),
        to: vec![],
        cc: vec![],
        subject: format!("Subject {uid}"),
        received_at: None,
        body_text: Some(format!("body of {uid}")),
        body_html: None,
        has_attachments: false,
        flags: vec![],
        labels: vec![],
        raw_ref: None,
        attachments: Vec::new(),
        raw: None,
    }
}

async fn poll(
    store: &Store,
    ctx: &AutomationContext,
    ws: WorkspaceId,
    automation: &catalerum_core::Automation,
) -> catalerum_ingest::CollectReport {
    let payload = CollectPayload::new(ws, automation.id, automation.triggers[0].clone());
    run_collect_email(store, ctx, ws, &payload)
        .await
        .expect("collect poll")
}

/// A Maildir (snapshot provider) message deleted at the source is reconciled on
/// the next poll: the stored row is hard-deleted (derived purge enqueued), the
/// survivor is kept, and a redelivered/no-change poll is a no-op (idempotent).
#[tokio::test]
async fn maildir_collect_reconciles_upstream_deletions_idempotently() {
    let Some(url) = db_url() else {
        eprintln!(
            "skipping maildir deletion test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
        );
        return;
    };
    let store = common::isolated_store(&url).await;
    let ws = store
        .workspaces()
        .create(
            "collectdel",
            &format!("collectdel-{}", uuid::Uuid::new_v4()),
        )
        .await
        .expect("ws");
    let runner: Arc<dyn ActionRunner> = Arc::new(SuccessRunner);
    let ctx = AutomationContext::new(runner);

    // A Maildir with two messages.
    let dir = tempfile::tempdir().expect("tempdir");
    for sub in ["new", "cur", "tmp"] {
        fs::create_dir_all(dir.path().join(sub)).unwrap();
    }
    write_message(dir.path(), "msg1", "One");
    write_message(dir.path(), "msg2", "Two");

    let connection = store
        .connections()
        .create(
            ws.id,
            ConnectionKind::Email,
            "md",
            None,
            Some(json!({ "root": dir.path().to_string_lossy() })),
        )
        .await
        .expect("connection");
    let conn = connection.id.to_string();

    // The automation runs under a grant scoped to exactly this connection —
    // also proving the §19 gate admits a covering grant end-to-end.
    let grant = store
        .grants()
        .upsert(
            ws.id,
            "collector",
            &[email_collect_cap(&conn)],
            &Constraints::default(),
        )
        .await
        .unwrap();
    let mut spec = collect_automation("mail-in", &conn);
    spec.grant_id = Some(grant.id);
    let automation = store.automations().create(ws.id, &spec).await.unwrap();

    // Poll 1: both messages are new → two per-item runs, nothing deleted.
    let report = poll(&store, &ctx, ws.id, &automation).await;
    assert_eq!(report.sources, 1);
    assert_eq!(report.runs_fired, 2);
    assert_eq!(report.deleted, 0);

    // Simulate the downstream WriteEmail: persist both items into the mailbox
    // row the collect upserted.
    let mailboxes = store.mailboxes().list_by_workspace(ws.id).await.unwrap();
    assert_eq!(mailboxes.len(), 1, "the collect poll upserted the mailbox");
    let mb = mailboxes[0].id;
    for uid in ["msg1", "msg2"] {
        store
            .emails()
            .upsert_by_uid(&stored_email(ws.id, mb, uid))
            .await
            .unwrap();
    }

    // The source deletes msg1. The next poll's full-snapshot diff reconciles:
    // the stored row is hard-deleted, the survivor kept, and no runs re-fire.
    fs::remove_file(dir.path().join("new").join("msg1")).unwrap();
    let report = poll(&store, &ctx, ws.id, &automation).await;
    assert_eq!(report.deleted, 1, "the vanished uid's row was reconciled");
    assert_eq!(report.runs_fired, 0, "the survivor is already committed");
    assert!(
        matches!(
            store.emails().get_by_uid(ws.id, mb, "msg1").await,
            Err(catalerum_store::StoreError::NotFound)
        ),
        "msg1's local row is gone"
    );
    assert!(
        store.emails().get_by_uid(ws.id, mb, "msg2").await.is_ok(),
        "msg2 survives"
    );

    // Poll 3 (no upstream change): the snapshot is unchanged, so nothing is
    // re-deleted or re-fired — the reconcile is idempotent.
    let report = poll(&store, &ctx, ws.id, &automation).await;
    assert_eq!(report.deleted, 0);
    assert_eq!(report.runs_fired, 0);
    assert!(store.emails().get_by_uid(ws.id, mb, "msg2").await.is_ok());
}

/// A two-event VCALENDAR (far-future starts, so the first poll's backfill
/// cutoff doesn't swallow them).
const ICS_TWO: &str = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
PRODID:-//catalerum//collect-test//EN\r
BEGIN:VEVENT\r
UID:standup@collect\r
DTSTART:20270613T090000Z\r
DTEND:20270613T093000Z\r
SUMMARY:Daily standup\r
END:VEVENT\r
BEGIN:VEVENT\r
UID:review@collect\r
DTSTART:20270613T140000Z\r
DTEND:20270613T150000Z\r
SUMMARY:Design review\r
END:VEVENT\r
END:VCALENDAR\r
";

/// The same VCALENDAR with the standup removed — an upstream deletion.
const ICS_ONE: &str = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
PRODID:-//catalerum//collect-test//EN\r
BEGIN:VEVENT\r
UID:review@collect\r
DTSTART:20270613T140000Z\r
DTEND:20270613T150000Z\r
SUMMARY:Design review\r
END:VEVENT\r
END:VCALENDAR\r
";

/// The calendar twin: a local `.ics` (snapshot provider) event removed at the
/// source is reconciled on the next collect poll — the stored event row is
/// hard-deleted (its `:Event` graph purge enqueued), the survivor kept,
/// idempotently.
#[tokio::test]
async fn local_ics_collect_reconciles_upstream_event_deletions() {
    let Some(url) = db_url() else {
        eprintln!("skipping ics deletion test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    let store = common::isolated_store(&url).await;
    let ws = store
        .workspaces()
        .create(
            "collectics",
            &format!("collectics-{}", uuid::Uuid::new_v4()),
        )
        .await
        .expect("ws");
    let runner: Arc<dyn ActionRunner> = Arc::new(SuccessRunner);
    let ctx = AutomationContext::new(runner);

    let dir = tempfile::tempdir().expect("tempdir");
    let cal_path = dir.path().join("work.ics");
    fs::write(&cal_path, ICS_TWO).unwrap();

    let connection = store
        .connections()
        .create(
            ws.id,
            ConnectionKind::Calendar,
            "ics",
            None,
            Some(json!({ "provider": "local", "path": dir.path().to_string_lossy() })),
        )
        .await
        .expect("connection");
    let conn = connection.id.to_string();

    let mut spec = NewAutomation {
        name: "cal-in".to_string(),
        enabled: true,
        triggers: vec![json!({ "kind": "collect_calendar", "connection": conn })],
        condition: None,
        actions: vec![json!({ "kind": "summarize" })],
        spec: None,
        grant_id: None,
    };
    // Run under a grant scoped to exactly this connection (the §19 collect
    // capability for calendars) — the covering-grant path, end-to-end.
    let grant = store
        .grants()
        .upsert(
            ws.id,
            "cal-collector",
            &[Capability::new(
                CapAction::Read,
                Resource::new("calendar", conn.clone()),
            )],
            &Constraints::default(),
        )
        .await
        .unwrap();
    spec.grant_id = Some(grant.id);
    let automation = store.automations().create(ws.id, &spec).await.unwrap();

    let poll_cal = |automation: catalerum_core::Automation| {
        let store = store.clone();
        let ctx = ctx.clone();
        async move {
            let payload = CollectPayload::new(ws.id, automation.id, automation.triggers[0].clone());
            run_collect_calendar(&store, &ctx, ws.id, &payload)
                .await
                .expect("collect poll")
        }
    };

    // Poll 1: both (future-dated) events fire runs; nothing deleted.
    let report = poll_cal(automation.clone()).await;
    assert_eq!(report.sources, 1);
    assert_eq!(report.runs_fired, 2);
    assert_eq!(report.deleted, 0);

    // Simulate the downstream WriteEvent for both items.
    let calendars = store.calendars().list_by_workspace(ws.id).await.unwrap();
    assert_eq!(calendars.len(), 1, "the collect poll upserted the calendar");
    let cal = calendars[0].id;
    for uid in ["standup@collect", "review@collect"] {
        store
            .events()
            .upsert_by_uid(&UpsertEvent {
                workspace_id: ws.id,
                calendar_id: cal,
                uid,
                starts_at: "2027-06-13T09:00:00Z".parse().unwrap(),
                ends_at: "2027-06-13T09:30:00Z".parse().unwrap(),
                all_day: false,
                rrule: None,
                summary: uid,
                location: None,
                body: None,
                attendees: &[],
                labels: &[],
                attachments: &[],
                etag: None,
                sequence: 0,
            })
            .await
            .unwrap();
    }

    // The source drops the standup. The next poll's full-snapshot diff
    // reconciles: its row is hard-deleted, the review survives, no re-fires.
    fs::write(&cal_path, ICS_ONE).unwrap();
    let report = poll_cal(automation.clone()).await;
    assert_eq!(report.deleted, 1, "the vanished uid's row was reconciled");
    assert_eq!(report.runs_fired, 0, "the survivor is already committed");
    assert!(
        matches!(
            store
                .events()
                .get_by_uid(ws.id, cal, "standup@collect")
                .await,
            Err(catalerum_store::StoreError::NotFound)
        ),
        "the standup's local row is gone"
    );
    assert!(
        store
            .events()
            .get_by_uid(ws.id, cal, "review@collect")
            .await
            .is_ok(),
        "the review survives"
    );

    // No-change poll: idempotent, nothing re-deleted.
    let report = poll_cal(automation).await;
    assert_eq!(report.deleted, 0);
    assert_eq!(report.runs_fired, 0);
}
