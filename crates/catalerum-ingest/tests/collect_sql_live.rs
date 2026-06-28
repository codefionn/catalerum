//! Live integration test for the `collect_sql` trigger (SOUL §11/§19): poll an
//! external Postgres database for newly-inserted rows in wildcard-matched
//! tables and fire one automation run per row.
//!
//! The "external" database is the test server itself (a unique schema per run),
//! reached through a real `postgres`-kind connection row whose password rides
//! the encrypted secret store — so the credential path, wildcard discovery,
//! cursor anchoring, per-row runs, dedup, and later-created-table pickup are
//! all exercised end-to-end.
//!
//! DB-gated like the other ingest tests: set `CATALERUM_TEST_DATABASE_URL` (or
//! `DATABASE_URL`) to run; otherwise it skips and passes offline.

mod common;

use std::sync::{Arc, Mutex};

use catalerum_automation::{Action, ActionOutcome, ActionRunner};
use catalerum_core::model::ConnectionKind;
use catalerum_core::WorkspaceId;
use catalerum_ingest::{run_collect_sql_with, AutomationContext, CollectPayload};
use catalerum_store::{NewAutomation, SecretStore};
use serde_json::{json, Value};
use sqlx::{Connection as _, Executor as _, PgConnection};

fn db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

/// Crack a `postgres://user[:pass]@host[:port]/db[?…]` test URL into the parts
/// the external-connection config needs. Deliberately simple — test URLs are.
fn parse_pg_url(url: &str) -> (String, u16, String, String, String) {
    let rest = url.split_once("://").map(|(_, r)| r).expect("scheme://");
    let rest = rest.split_once('?').map_or(rest, |(r, _)| r);
    let (userinfo, hostpath) = rest.split_once('@').expect("user@host");
    let (user, pass) = match userinfo.split_once(':') {
        Some((u, p)) => (u.to_string(), p.to_string()),
        None => (userinfo.to_string(), String::new()),
    };
    let (hostport, db) = hostpath.split_once('/').expect("host/db");
    let (host, port) = match hostport.split_once(':') {
        Some((h, p)) => (h.to_string(), p.parse::<u16>().expect("port")),
        None => (hostport.to_string(), 5432),
    };
    (host, port, db.to_string(), user, pass)
}

/// A runner that succeeds every action and records the input envelope each
/// per-row run received, so the test can assert the row rode the trigger.
struct RecordingRunner {
    seen: Mutex<Vec<Value>>,
}

#[async_trait::async_trait]
impl ActionRunner for RecordingRunner {
    async fn run(
        &self,
        _workspace_id: WorkspaceId,
        _action: &Action,
        trigger: Option<&Value>,
        _grant: Option<&catalerum_core::model::Grant>,
    ) -> ActionOutcome {
        if let Some(t) = trigger {
            self.seen.lock().unwrap().push(t.clone());
        }
        ActionOutcome::succeeded(None)
    }
}

#[tokio::test]
async fn collect_sql_fires_one_run_per_new_row_in_wildcard_tables() {
    let Some(url) = db_url() else {
        eprintln!(
            "skipping collect_sql live test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
        );
        return;
    };
    let store = common::isolated_store(&url).await;
    let ws = store
        .workspaces()
        .create("csql", &format!("csql-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");

    // The "external" database: the shared test server, one unique schema per run.
    let (host, port, database, username, password) = parse_pg_url(&url);
    let schema = format!("csql_{}", uuid::Uuid::new_v4().simple());
    let mut ext = PgConnection::connect(&url).await.expect("external conn");
    ext.execute(format!(r#"CREATE SCHEMA "{schema}""#).as_str())
        .await
        .expect("create schema");
    for table in ["orders_a", "orders_b"] {
        ext.execute(
            format!(r#"CREATE TABLE "{schema}"."{table}" (id bigserial PRIMARY KEY, note text)"#)
                .as_str(),
        )
        .await
        .expect("create table");
    }
    // Pre-existing rows must never fire (the first poll anchors, not replays).
    ext.execute(format!(r#"INSERT INTO "{schema}"."orders_a" (note) VALUES ('old')"#).as_str())
        .await
        .expect("seed row");

    // The connection row: config in the clear, password sealed in the secret store.
    let secrets =
        Arc::new(SecretStore::new(store.pool().clone(), &[7u8; 32]).expect("secret store"));
    let credential_ref = secrets.put(ws.id, password.as_bytes()).await.expect("seal");
    let config = json!({
        "host": host, "port": port, "database": database, "username": username,
    });
    let connection = store
        .connections()
        .create(
            ws.id,
            ConnectionKind::Postgres,
            "extdb",
            Some(&credential_ref),
            Some(config),
        )
        .await
        .expect("connection");

    let trigger = json!({
        "kind": "collect_sql",
        "connection": connection.id.to_string(),
        "tables": format!("{schema}.orders_*"),
    });
    let automation = store
        .automations()
        .create(
            ws.id,
            &NewAutomation {
                name: "on-new-row".to_string(),
                enabled: true,
                triggers: vec![trigger.clone()],
                condition: None,
                actions: vec![json!({ "kind": "summarize" })],
                spec: None,
                grant_id: None,
            },
        )
        .await
        .expect("automation");

    let runner = Arc::new(RecordingRunner {
        seen: Mutex::new(Vec::new()),
    });
    let ctx = AutomationContext::new(runner.clone() as Arc<dyn ActionRunner>);
    let payload = CollectPayload::new(ws.id, automation.id, trigger);
    let poll = || run_collect_sql_with(&store, &ctx, ws.id, &payload, Some(&secrets));

    // Poll 1: both tables discovered, cursors anchored, nothing fires.
    let report = poll().await.expect("anchor poll");
    assert_eq!(report.sources, 2, "both wildcard tables discovered");
    assert_eq!(report.runs_fired, 0, "pre-existing rows never fire");

    // New inserts across both tables → one run per row, all committed.
    ext.execute(
        format!(r#"INSERT INTO "{schema}"."orders_a" (note) VALUES ('a1'), ('a2'), ('a3')"#)
            .as_str(),
    )
    .await
    .expect("insert a");
    ext.execute(format!(r#"INSERT INTO "{schema}"."orders_b" (note) VALUES ('b1')"#).as_str())
        .await
        .expect("insert b");
    let report = poll().await.expect("poll 2");
    assert_eq!(report.runs_fired, 4, "one run per newly-inserted row");
    assert_eq!(report.committed, 4, "fire-and-forget commits every row");
    {
        let seen = runner.seen.lock().unwrap();
        let notes: Vec<&str> = seen
            .iter()
            .filter_map(|t| t.pointer("/row/note")?.as_str())
            .collect();
        assert_eq!(
            notes,
            vec!["a1", "a2", "a3", "b1"],
            "each run's trigger carries its row (a-table rows in insertion order first)"
        );
        assert!(
            seen.iter()
                .all(|t| t.pointer("/kind").and_then(Value::as_str) == Some("collect_sql")),
            "trigger kind rides each run"
        );
    }

    // Re-poll with no new rows: the advanced cursor + dedup fire nothing.
    let report = poll().await.expect("poll 3");
    assert_eq!(report.runs_fired, 0, "no re-fire without new inserts");

    // A table created AFTER wiring that matches the wildcard joins automatically:
    // first sight anchors it, then its inserts fire.
    ext.execute(
        format!(r#"CREATE TABLE "{schema}"."orders_c" (id bigserial PRIMARY KEY, note text)"#)
            .as_str(),
    )
    .await
    .expect("late table");
    let report = poll().await.expect("poll 4");
    assert_eq!(report.sources, 3, "the late-created table is discovered");
    assert_eq!(report.runs_fired, 0, "its first sight anchors, never fires");
    ext.execute(format!(r#"INSERT INTO "{schema}"."orders_c" (note) VALUES ('c1')"#).as_str())
        .await
        .expect("insert c");
    let report = poll().await.expect("poll 5");
    assert_eq!(report.runs_fired, 1, "the late table's new row fires");

    // Best-effort cleanup of the shared server's schema.
    let _ = ext
        .execute(format!(r#"DROP SCHEMA "{schema}" CASCADE"#).as_str())
        .await;
    let _ = ext.close().await;
}
