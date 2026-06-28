//! Per-test database isolation for the DB-backed `--lib` tests.
//!
//! A couple of tests dispatch a trigger and then spin a `SyncWorker` that drains
//! Postgres' **global** `job_queue` (`dequeue_one` claims the oldest pending job
//! regardless of workspace). Under the parallel test runner one test's worker can
//! claim another test's `run_automation` job — so they flake intermittently while
//! passing solo / `--test-threads=1`. [`isolated_store`] gives a test its **own**
//! ephemeral database (own `job_queue`), removing the contention by construction.
//! Mirrors the `catalerum-ingest` integration-test helper (`tests/common`).

use std::time::Duration;

use catalerum_store::{PoolConfig, Store};
use sqlx::{Connection, Executor, PgConnection};

/// Connect to a freshly-created, isolated database derived from `base_url`'s
/// server. The throwaway `cit_*` db is intentionally leaked — `just test` runs on
/// an ephemeral Postgres, and on a persistent dev db the leftovers are harmless
/// clutter cleared by recreating it / `just reset`.
pub(crate) async fn isolated_store(base_url: &str) -> Store {
    let db_name = format!("cit_{}", uuid::Uuid::new_v4().simple());

    // `CREATE DATABASE` cannot run in a transaction and needs a connection to a
    // *different* database, so use the server's default `postgres` db.
    let admin_url = swap_db(base_url, "postgres");
    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("connect to maintenance db for CREATE DATABASE");
    admin
        .execute(format!(r#"CREATE DATABASE "{db_name}""#).as_str())
        .await
        .expect("CREATE DATABASE for isolated test");
    let _ = admin.close().await;

    // Migrate + connect to the new db with a small pool (many parallel tests must
    // not exhaust the server's `max_connections`).
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
                "postgres://catalerum:catalerum@localhost:5432/catalerum",
                "postgres"
            ),
            "postgres://catalerum:catalerum@localhost:5432/postgres"
        );
        assert_eq!(
            swap_db("postgres://u:p@h:5432/db?sslmode=disable", "cit_abc"),
            "postgres://u:p@h:5432/cit_abc?sslmode=disable"
        );
    }
}
