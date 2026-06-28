//! Shared helpers for the ingest integration tests.
//!
//! # Per-test database isolation
//! The worker-driving tests share Postgres' **global `job_queue`**
//! (`dequeue_one` claims the oldest pending job regardless of workspace) and run
//! as **parallel test binaries**. So one test's worker can claim another test's
//! job, fail it for lack of the right worker context, and trip its 10s
//! exponential backoff — the residual the per-binary `queue_lock` mutex in
//! `worker_queue.rs` could never fully cover (its own comment: "without a per-test
//! database").
//!
//! [`isolated_store`] gives each test its **own ephemeral database** (own
//! `job_queue`), eliminating cross-test contention by construction — no
//! serialization, no foreign-job claiming. It `CREATE DATABASE`s a uniquely-named
//! db on the same server, migrates it, and returns a [`Store`] for it with a
//! small connection pool (so N parallel tests don't exhaust `max_connections`).
//! The throwaway db is intentionally **leaked**: under `just test` the whole
//! ephemeral Postgres is discarded; for a persistent dev Postgres the `cit_*`
//! databases are harmless clutter cleared by recreating it / `just reset`.

#![allow(dead_code)] // not every test binary uses this helper

use std::time::Duration;

use catalerum_store::{PoolConfig, Store};
use sqlx::{Connection, Executor, PgConnection};

/// Connect to an **isolated**, freshly-created database derived from `base_url`'s
/// server. Each call returns a [`Store`] whose `job_queue` is private to this
/// test, so parallel worker tests never claim each other's jobs.
pub async fn isolated_store(base_url: &str) -> Store {
    let db_name = format!("cit_{}", uuid::Uuid::new_v4().simple());

    // `CREATE DATABASE` cannot run inside a transaction and needs a connection to
    // a *different* database, so use the server's default `postgres` db.
    let admin_url = swap_db(base_url, "postgres");
    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("connect to maintenance db for CREATE DATABASE");
    admin
        .execute(format!(r#"CREATE DATABASE "{db_name}""#).as_str())
        .await
        .expect("CREATE DATABASE for isolated test");
    let _ = admin.close().await;

    // Migrate + connect to the new db with a small pool (many parallel tests).
    let test_url = swap_db(base_url, &db_name);
    Store::connect_with(
        &test_url,
        &PoolConfig {
            max_connections: 2,
            min_connections: 0,
            acquire_timeout: Duration::from_secs(30),
        },
    )
    .await
    .expect("connect+migrate isolated test db")
}

/// Replace the database name in a `postgres://…/<db>[?params]` URL.
fn swap_db(url: &str, new_db: &str) -> String {
    let (base, params) = match url.split_once('?') {
        Some((b, p)) => (b, format!("?{p}")),
        None => (url, String::new()),
    };
    // The last `/` separates the authority (`scheme://user:pw@host:port`) from the
    // db path segment.
    let prefix = base.rsplit_once('/').map_or(base, |(p, _)| p);
    format!("{prefix}/{new_db}{params}")
}

#[cfg(test)]
mod tests {
    use super::swap_db;

    #[test]
    fn swap_db_replaces_only_the_db_segment() {
        assert_eq!(
            swap_db(
                "postgres://postgres:pw@127.0.0.1:55440/catalerum",
                "postgres"
            ),
            "postgres://postgres:pw@127.0.0.1:55440/postgres"
        );
        assert_eq!(
            swap_db("postgres://u:p@h:5432/db?sslmode=disable", "cit_abc"),
            "postgres://u:p@h:5432/cit_abc?sslmode=disable"
        );
    }
}
