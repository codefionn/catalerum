//! Integration test: a backup → wipe → restore round-trip and retention pruning
//! (SOUL §30), against an **isolated** ephemeral Postgres database so the
//! destructive truncate/restore never touches a shared dev DB.
//!
//! Same DB gating as the store/ingest tests: set `CATALERUM_TEST_DATABASE_URL`
//! (or `DATABASE_URL`) to run it; otherwise it skips and passes so the suite
//! stays green offline.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use sqlx::{Connection, Executor, PgConnection};

use catalerum_backup::BackupEngine;
use catalerum_core::model::Author;
use catalerum_core::provider::{PutMeta, StorageBackend};
use catalerum_core::UserId;
use catalerum_storage::LocalFsBackend;
use catalerum_store::Store;

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

/// Replace the database name in a `postgres://…/<db>[?params]` URL.
fn swap_db(url: &str, new_db: &str) -> String {
    let (base, params) = match url.split_once('?') {
        Some((b, p)) => (b, format!("?{p}")),
        None => (url, String::new()),
    };
    let prefix = base.rsplit_once('/').map_or(base, |(p, _)| p);
    format!("{prefix}/{new_db}{params}")
}

/// A fresh, migrated, isolated database — restore truncates everything, so it
/// must not be a shared DB.
async fn isolated_store(base_url: &str) -> (Store, String) {
    let db_name = format!("bkp_{}", uuid::Uuid::new_v4().simple());
    let admin_url = swap_db(base_url, "postgres");
    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("connect to maintenance db");
    admin
        .execute(format!(r#"CREATE DATABASE "{db_name}""#).as_str())
        .await
        .expect("CREATE DATABASE");
    let _ = admin.close().await;
    let store = Store::connect(&swap_db(base_url, &db_name))
        .await
        .expect("connect+migrate isolated db");
    (store, db_name)
}

async fn put_blob(backend: &dyn StorageBackend, key: &str, body: &[u8]) {
    let bytes = body.to_vec();
    let stream = futures::stream::once(async move { Ok(bytes) }).boxed();
    backend
        .put(key, stream, PutMeta::default())
        .await
        .expect("put blob");
}

async fn read_blob(backend: &dyn StorageBackend, key: &str) -> Vec<u8> {
    let mut stream = backend.get(key).await.expect("get blob");
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        buf.extend_from_slice(&chunk.expect("chunk"));
    }
    buf
}

async fn count_notes(store: &Store) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM notes")
        .fetch_one(store.pool())
        .await
        .expect("count notes")
}

#[tokio::test]
async fn backup_restore_round_trips_postgres_and_blobs() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping backup_restore_round_trips_postgres_and_blobs: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };

    let (store, _db) = isolated_store(&url).await;

    // --- seed: a workspace, a note, and a stored blob -----------------------
    let ws = store
        .workspaces()
        .create("backup", &format!("backup-{}", uuid::Uuid::new_v4()))
        .await
        .expect("workspace");
    store
        .notes()
        .create(
            ws.id,
            Author::User { id: UserId::new() },
            "Recoverable",
            "this note must survive a restore",
            &["backup".to_string()],
        )
        .await
        .expect("note");
    assert_eq!(count_notes(&store).await, 1);

    let tmp = std::env::temp_dir().join(format!("catalerum-backup-{}", uuid::Uuid::new_v4()));
    let source: Arc<dyn StorageBackend> = Arc::new(LocalFsBackend::new(tmp.join("live")));
    let dest: Arc<dyn StorageBackend> = Arc::new(LocalFsBackend::new(tmp.join("dest")));
    let blob_key = format!("{}/report.txt", ws.id);
    put_blob(
        source.as_ref(),
        &blob_key,
        b"blob bytes that live only in storage",
    )
    .await;

    let engine = BackupEngine::new(store.pool().clone(), dest.clone(), "test")
        .with_prefix("backups")
        .with_source_storage(source.clone())
        .with_keep(1);

    // --- back up ------------------------------------------------------------
    let summary = engine.run().await.expect("backup run");
    assert!(summary.tables > 0, "should dump the migrated tables");
    assert!(summary.rows >= 1, "should dump at least the seeded note");
    assert_eq!(summary.objects, 1, "should copy the one stored blob");

    // The manifest is readable and names the dumped tables + the blob.
    let manifest = engine.read_manifest(&summary.id).await.expect("manifest");
    assert_eq!(manifest.format_version, catalerum_backup::FORMAT_VERSION);
    assert!(manifest.objects.included && manifest.objects.count == 1);
    assert!(manifest.postgres.tables.iter().any(|t| t.name == "notes"));

    // --- destroy live state -------------------------------------------------
    store
        .pool()
        .execute("DELETE FROM notes")
        .await
        .expect("wipe notes");
    assert_eq!(count_notes(&store).await, 0);
    source.delete(&blob_key).await.expect("delete live blob");
    assert!(source.stat(&blob_key).await.is_err(), "blob is gone");

    // --- restore ------------------------------------------------------------
    let restored = engine.restore(&summary.id, false).await.expect("restore");
    assert_eq!(restored.rows, summary.rows, "all rows reloaded");
    assert_eq!(restored.objects, 1, "the blob is restored");
    assert_eq!(count_notes(&store).await, 1, "the note is back");
    let meta = source.stat(&blob_key).await.expect("blob restored");
    assert!(meta.size > 0);

    // --- retention: a second backup, then prune keeps only the newest -------
    // Backup ids are second-granular timestamps; wait so the two ids differ.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let second = engine.run().await.expect("second backup");
    assert_ne!(second.id, summary.id);
    let mut ids = engine.list().await.expect("list");
    ids.sort();
    assert_eq!(ids.len(), 2, "two backups present before prune");

    let pruned = engine.prune().await.expect("prune");
    assert_eq!(pruned, 1, "keep=1 prunes the older backup");
    let remaining = engine.list().await.expect("list after prune");
    assert_eq!(remaining, vec![second.id], "only the newest survives");

    let _ = tokio::fs::remove_dir_all(&tmp).await;
}

/// Regression guard for the 2026-07-02 schema additions (migrations 0046–0053),
/// which introduced foreign-key edges the original restore ordering never saw:
/// `workspaces.organisation_id` → `organisations` (0046), the `org_memberships`
/// composite key (0046), the `sessions (workspace_id, grant_id)` composite FK →
/// `grants` (0050), `mcp_endpoints` → workspaces+grants (0047), `app_data` →
/// workspaces (0051), `emails.attachments` JSONB (0049), soft-`archived_at`
/// workspaces (0048), and pod-scoped rows + `pod_heartbeats` (0052/0053).
///
/// It proves the restore is **order-independent**: the dump is taken from one
/// database, then reloaded into a *fresh, pristine, migrated* database (not the
/// same one wiped) via `SET session_replication_role = replica`, and every row —
/// child before parent or not — lands with its foreign keys intact. A FUTURE
/// migration that adds a child table whose parent sorts *after* it alphabetically
/// (dumped/loaded before its parent) would still pass here only because FK
/// triggers are disabled for the load; if someone ever removes that mechanism,
/// this test fails. Gated on a live Postgres; skips offline.
#[tokio::test]
async fn backup_restore_round_trips_todays_fk_schema() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping backup_restore_round_trips_todays_fk_schema: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };

    // Fixed ids so the assertions can name exactly the rows we seeded.
    const ORG: &str = "0a9b0000-0000-4000-8000-000000000001";
    const USER: &str = "0a9b0000-0000-4000-8000-000000000002";
    const WS_ACTIVE: &str = "0a9b0000-0000-4000-8000-000000000003";
    const WS_ARCHIVED: &str = "0a9b0000-0000-4000-8000-000000000004";
    const GRANT: &str = "0a9b0000-0000-4000-8000-000000000005";
    const SESSION: &str = "0a9b0000-0000-4000-8000-000000000006";
    const CONN: &str = "0a9b0000-0000-4000-8000-000000000007";
    const MAILBOX: &str = "0a9b0000-0000-4000-8000-000000000008";
    const EMAIL: &str = "0a9b0000-0000-4000-8000-000000000009";
    const MCP: &str = "0a9b0000-0000-4000-8000-00000000000a";
    const TERM: &str = "0a9b0000-0000-4000-8000-00000000000b";
    const POD: &str = "pod-backup-roundtrip";

    // A single unprepared batch (simple query protocol) exercising every FK edge
    // that landed 2026-07-02. Ordering here is child-after-parent for the *insert*
    // (FK triggers are on for a normal insert); the whole point is that the
    // *restore* need not preserve it.
    let seed = format!(
        r#"
        INSERT INTO users (id, email, display_name)
            VALUES ('{USER}', 'roundtrip@example.com', 'Roundtrip User');
        INSERT INTO organisations (id, name, slug, workspace_creation)
            VALUES ('{ORG}', 'Roundtrip Org', 'roundtrip-org', 'admins');
        INSERT INTO org_memberships (organisation_id, user_id, role)
            VALUES ('{ORG}', '{USER}', 'owner');
        INSERT INTO workspaces (id, name, slug, organisation_id)
            VALUES ('{WS_ACTIVE}', 'Active WS', 'active-ws', '{ORG}');
        INSERT INTO workspaces (id, name, slug, organisation_id, archived_at)
            VALUES ('{WS_ARCHIVED}', 'Archived WS', 'archived-ws', '{ORG}', now());
        INSERT INTO memberships (workspace_id, user_id, role)
            VALUES ('{WS_ACTIVE}', '{USER}', 'owner');
        INSERT INTO grants (id, workspace_id, name, capabilities, constraints)
            VALUES ('{GRANT}', '{WS_ACTIVE}', 'scoped',
                    '["notes:read"]'::jsonb, '{{"net":"deny"}}'::jsonb);
        INSERT INTO sessions (id, workspace_id, user_id, token_hash, expires_at, grant_id)
            VALUES ('{SESSION}', '{WS_ACTIVE}', '{USER}', 'tokenhash',
                    now() + interval '1 day', '{GRANT}');
        INSERT INTO app_data (workspace_id, app, key, value)
            VALUES ('{WS_ACTIVE}', 'habit-tracker', 'state',
                    '{{"streak":7,"nested":{{"a":[1,2,3]}}}}'::jsonb);
        INSERT INTO connections (id, workspace_id, kind, name, credential_ref, config)
            VALUES ('{CONN}', '{WS_ACTIVE}', 'imap', 'Mail', 'cred-ref-123',
                    '{{"host":"imap.example.com"}}'::jsonb);
        INSERT INTO mailboxes (id, workspace_id, connection_id, external_id, name)
            VALUES ('{MAILBOX}', '{WS_ACTIVE}', '{CONN}', 'INBOX', 'Inbox');
        INSERT INTO emails (id, workspace_id, mailbox_id, uid, message_id, subject,
                            attachments, has_attachments)
            VALUES ('{EMAIL}', '{WS_ACTIVE}', '{MAILBOX}', '1',
                    '<msg-1@example.com>', 'Hello',
                    '[{{"url":"/storage/objects/att1","name":"a.pdf","size":123}}]'::jsonb,
                    true);
        INSERT INTO mcp_endpoints (id, workspace_id, name, author_kind, author_id, grant_id)
            VALUES ('{MCP}', '{WS_ACTIVE}', 'search', 'user', '{USER}', '{GRANT}');
        INSERT INTO pod_heartbeats (pod_id) VALUES ('{POD}');
        INSERT INTO terminal_sessions (id, workspace_id, backend, pod_id)
            VALUES ('{TERM}', '{WS_ACTIVE}', 'pty', '{POD}');
        INSERT INTO workspace_sandboxes (workspace_id, backend, image, pod_id)
            VALUES ('{WS_ACTIVE}', 'podman', 'img:latest', '{POD}');
        "#
    );

    // --- source DB: seed + back up ------------------------------------------
    let (src, _src_db) = isolated_store(&url).await;
    src.pool()
        .execute(seed.as_str())
        .await
        .expect("seed schema");

    let tmp = std::env::temp_dir().join(format!("catalerum-backup-fk-{}", uuid::Uuid::new_v4()));
    let dest: Arc<dyn StorageBackend> = Arc::new(LocalFsBackend::new(tmp.join("dest")));

    // Postgres-only backup (no source storage attached → objects excluded).
    let engine_src =
        BackupEngine::new(src.pool().clone(), dest.clone(), "test").with_prefix("backups");
    let summary = engine_src.run().await.expect("backup run");
    assert!(summary.tables > 0);

    // Every seeded table must be in the dump (no allow/deny list silently skips a
    // new table). If any of these is missing, it was never backed up.
    let manifest = engine_src
        .read_manifest(&summary.id)
        .await
        .expect("manifest");
    let dumped: std::collections::HashSet<&str> = manifest
        .postgres
        .tables
        .iter()
        .map(|t| t.name.as_str())
        .collect();
    for t in [
        "organisations",
        "org_memberships",
        "workspaces",
        "grants",
        "sessions",
        "app_data",
        "connections",
        "mailboxes",
        "emails",
        "mcp_endpoints",
        "pod_heartbeats",
        "terminal_sessions",
        "workspace_sandboxes",
    ] {
        assert!(
            dumped.contains(t),
            "table `{t}` must be in the backup manifest"
        );
    }

    // --- restore into a FRESH, pristine, migrated DB (not the same one wiped) --
    let (dst, _dst_db) = isolated_store(&url).await;
    let engine_dst =
        BackupEngine::new(dst.pool().clone(), dest.clone(), "test").with_prefix("backups");
    engine_dst
        .restore(&summary.id, false)
        .await
        .expect("restore into fresh db");

    let pool = dst.pool();
    let scalar_i64 = |sql: String| async move {
        sqlx::query_scalar::<_, i64>(&sql)
            .fetch_one(pool)
            .await
            .expect("scalar query")
    };

    // The org + the workspace's FK to it (0046).
    assert_eq!(
        scalar_i64(
            "SELECT count(*) FROM workspaces w JOIN organisations o \
               ON w.organisation_id = o.id WHERE o.slug = 'roundtrip-org'"
                .to_string()
        )
        .await,
        2,
        "both workspaces resolve their organisation_id FK (0046)"
    );
    // Soft-archive column (0048): exactly one archived, one active.
    assert_eq!(
        scalar_i64(format!(
            "SELECT count(*) FROM workspaces WHERE id = '{WS_ARCHIVED}' AND archived_at IS NOT NULL"
        ))
        .await,
        1,
        "archived_at survived (0048)"
    );
    // org_memberships composite key (0046).
    assert_eq!(
        scalar_i64(
            "SELECT count(*) FROM org_memberships m JOIN organisations o \
               ON m.organisation_id = o.id JOIN users u ON m.user_id = u.id \
             WHERE o.slug = 'roundtrip-org'"
                .to_string()
        )
        .await,
        1,
        "org_membership resolves both its FKs (0046)"
    );
    // The composite `sessions (workspace_id, grant_id)` → `grants` FK (0050): the
    // grant-scoped session must join to its grant in the SAME workspace.
    assert_eq!(
        scalar_i64(format!(
            "SELECT count(*) FROM sessions s JOIN grants g \
               ON s.workspace_id = g.workspace_id AND s.grant_id = g.id \
             WHERE s.id = '{SESSION}'"
        ))
        .await,
        1,
        "grant-scoped session resolves its composite FK (0050)"
    );
    // No orphaned grant-scoped session (the FK truly holds after the trigger-off load).
    assert_eq!(
        scalar_i64(
            "SELECT count(*) FROM sessions s \
             LEFT JOIN grants g ON s.workspace_id = g.workspace_id AND s.grant_id = g.id \
             WHERE s.grant_id IS NOT NULL AND g.id IS NULL"
                .to_string()
        )
        .await,
        0,
        "no grant-scoped session is orphaned after restore"
    );
    // mcp_endpoints → workspaces + grants (0047).
    assert_eq!(
        scalar_i64(format!(
            "SELECT count(*) FROM mcp_endpoints e JOIN workspaces w ON e.workspace_id = w.id \
             JOIN grants g ON e.grant_id = g.id WHERE e.id = '{MCP}'"
        ))
        .await,
        1,
        "mcp_endpoint resolves its workspace + grant FKs (0047)"
    );
    // app_data → workspaces (0051), with its nested JSONB value intact.
    assert_eq!(
        scalar_i64(
            "SELECT count(*) FROM app_data d JOIN workspaces w ON d.workspace_id = w.id \
             WHERE d.app = 'habit-tracker' AND d.key = 'state' \
               AND d.value #>> '{nested,a,2}' = '3'"
                .to_string()
        )
        .await,
        1,
        "app_data row + nested JSONB survived and resolves its workspace FK (0051)"
    );
    // emails: message_id + non-empty attachments JSONB (0049), FK chain to mailbox→connection.
    assert_eq!(
        scalar_i64(format!(
            "SELECT count(*) FROM emails em JOIN mailboxes mb ON em.mailbox_id = mb.id \
             JOIN connections c ON mb.connection_id = c.id \
             WHERE em.id = '{EMAIL}' AND em.message_id = '<msg-1@example.com>' \
               AND jsonb_array_length(em.attachments) = 1 \
               AND em.attachments #>> '{{0,name}}' = 'a.pdf'"
        ))
        .await,
        1,
        "email message_id + attachments JSONB survived and resolves its FK chain (0049)"
    );
    // Encrypted-credential ref (a nullable TEXT that also proves the connection row loaded).
    assert_eq!(
        scalar_i64(format!(
            "SELECT count(*) FROM connections WHERE id = '{CONN}' AND credential_ref = 'cred-ref-123'"
        ))
        .await,
        1,
        "connection credential_ref survived"
    );
    // pod-scoped rows + pod_heartbeats (0052/0053).
    assert_eq!(
        scalar_i64(format!(
            "SELECT count(*) FROM pod_heartbeats WHERE pod_id = '{POD}'"
        ))
        .await,
        1,
        "pod_heartbeat survived (0053)"
    );
    assert_eq!(
        scalar_i64(format!(
            "SELECT count(*) FROM terminal_sessions WHERE id = '{TERM}' AND pod_id = '{POD}'"
        ))
        .await,
        1,
        "terminal_session pod_id survived (0052)"
    );
    assert_eq!(
        scalar_i64(format!(
            "SELECT count(*) FROM workspace_sandboxes WHERE workspace_id = '{WS_ACTIVE}' AND pod_id = '{POD}'"
        ))
        .await,
        1,
        "workspace_sandbox pod_id survived (0052)"
    );

    let _ = tokio::fs::remove_dir_all(&tmp).await;
}

/// The **non-superuser** restore path: a hardened, least-privilege DB role cannot
/// `SET session_replication_role = replica` (a superuser-only GUC — it errors with
/// SQLSTATE 42501), so restore must instead load tables in foreign-key dependency
/// order (parents before children) with the FKs still enforced.
///
/// The dev/CI Postgres role IS a superuser, so we force that path with
/// [`BackupEngine::with_force_ordered_restore`] to prove it deterministically:
/// the exact same interconnected FK graph (multi-level chains org→workspace→grant,
/// workspace→connection→mailbox→email, plus the **composite** `sessions
/// (workspace_id, grant_id)` → `grants` FK) round-trips with every foreign key
/// intact when loaded parents-first — not by disabled triggers. If the topological
/// sort ever failed to order this graph, the trigger-on `COPY` would raise a
/// foreign-key violation and this test would fail. Gated on a live Postgres; skips
/// offline.
#[tokio::test]
async fn backup_restore_dependency_ordered_path_loads_fk_graph() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping backup_restore_dependency_ordered_path_loads_fk_graph: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };

    const ORG: &str = "0b9c0000-0000-4000-8000-000000000001";
    const USER: &str = "0b9c0000-0000-4000-8000-000000000002";
    const WS: &str = "0b9c0000-0000-4000-8000-000000000003";
    const GRANT: &str = "0b9c0000-0000-4000-8000-000000000004";
    const SESSION: &str = "0b9c0000-0000-4000-8000-000000000005";
    const CONN: &str = "0b9c0000-0000-4000-8000-000000000006";
    const MAILBOX: &str = "0b9c0000-0000-4000-8000-000000000007";
    const EMAIL: &str = "0b9c0000-0000-4000-8000-000000000008";
    const MCP: &str = "0b9c0000-0000-4000-8000-000000000009";

    // An interconnected graph: two independent chains off `workspaces`, plus the
    // composite grant-scoped session and a diamond (mcp_endpoints → workspace +
    // grant). Insert order here is child-after-parent for the seed's own FK checks;
    // the restore proves that order is *reconstructed* topologically.
    let seed = format!(
        r#"
        INSERT INTO users (id, email, display_name)
            VALUES ('{USER}', 'ordered@example.com', 'Ordered User');
        INSERT INTO organisations (id, name, slug, workspace_creation)
            VALUES ('{ORG}', 'Ordered Org', 'ordered-org', 'admins');
        INSERT INTO org_memberships (organisation_id, user_id, role)
            VALUES ('{ORG}', '{USER}', 'owner');
        INSERT INTO workspaces (id, name, slug, organisation_id)
            VALUES ('{WS}', 'Ordered WS', 'ordered-ws', '{ORG}');
        INSERT INTO memberships (workspace_id, user_id, role)
            VALUES ('{WS}', '{USER}', 'owner');
        INSERT INTO grants (id, workspace_id, name, capabilities, constraints)
            VALUES ('{GRANT}', '{WS}', 'scoped',
                    '["notes:read"]'::jsonb, '{{"net":"deny"}}'::jsonb);
        INSERT INTO sessions (id, workspace_id, user_id, token_hash, expires_at, grant_id)
            VALUES ('{SESSION}', '{WS}', '{USER}', 'tokenhash',
                    now() + interval '1 day', '{GRANT}');
        INSERT INTO connections (id, workspace_id, kind, name, credential_ref, config)
            VALUES ('{CONN}', '{WS}', 'imap', 'Mail', 'cred-ref-9',
                    '{{"host":"imap.example.com"}}'::jsonb);
        INSERT INTO mailboxes (id, workspace_id, connection_id, external_id, name)
            VALUES ('{MAILBOX}', '{WS}', '{CONN}', 'INBOX', 'Inbox');
        INSERT INTO emails (id, workspace_id, mailbox_id, uid, message_id, subject,
                            attachments, has_attachments)
            VALUES ('{EMAIL}', '{WS}', '{MAILBOX}', '1',
                    '<ordered-1@example.com>', 'Hello', '[]'::jsonb, false);
        INSERT INTO mcp_endpoints (id, workspace_id, name, author_kind, author_id, grant_id)
            VALUES ('{MCP}', '{WS}', 'search', 'user', '{USER}', '{GRANT}');
        "#
    );

    // --- source DB: seed + back up ------------------------------------------
    let (src, _src_db) = isolated_store(&url).await;
    src.pool()
        .execute(seed.as_str())
        .await
        .expect("seed schema");

    let tmp = std::env::temp_dir().join(format!("catalerum-backup-ord-{}", uuid::Uuid::new_v4()));
    let dest: Arc<dyn StorageBackend> = Arc::new(LocalFsBackend::new(tmp.join("dest")));
    let engine_src =
        BackupEngine::new(src.pool().clone(), dest.clone(), "test").with_prefix("backups");
    let summary = engine_src.run().await.expect("backup run");
    assert!(summary.rows >= 1);

    // --- restore into a FRESH DB, FORCING the dependency-ordered path -------
    let (dst, _dst_db) = isolated_store(&url).await;
    let engine_dst = BackupEngine::new(dst.pool().clone(), dest.clone(), "test")
        .with_prefix("backups")
        .with_force_ordered_restore(true);
    let restored = engine_dst
        .restore(&summary.id, false)
        .await
        .expect("dependency-ordered restore into fresh db");
    assert_eq!(
        restored.rows, summary.rows,
        "the ordered path loads every row the dump held"
    );

    let pool = dst.pool();
    let scalar_i64 = |sql: String| async move {
        sqlx::query_scalar::<_, i64>(&sql)
            .fetch_one(pool)
            .await
            .expect("scalar query")
    };

    // The full chain org→workspace resolves (parent loaded before child).
    assert_eq!(
        scalar_i64(
            "SELECT count(*) FROM workspaces w JOIN organisations o \
               ON w.organisation_id = o.id WHERE o.slug = 'ordered-org'"
                .to_string()
        )
        .await,
        1,
        "workspace resolves its organisation FK under the ordered load"
    );
    // The composite (workspace_id, grant_id) → grants FK — the trickiest edge.
    assert_eq!(
        scalar_i64(format!(
            "SELECT count(*) FROM sessions s JOIN grants g \
               ON s.workspace_id = g.workspace_id AND s.grant_id = g.id \
             WHERE s.id = '{SESSION}'"
        ))
        .await,
        1,
        "grant-scoped session resolves its composite FK under the ordered load"
    );
    assert_eq!(
        scalar_i64(
            "SELECT count(*) FROM sessions s \
             LEFT JOIN grants g ON s.workspace_id = g.workspace_id AND s.grant_id = g.id \
             WHERE s.grant_id IS NOT NULL AND g.id IS NULL"
                .to_string()
        )
        .await,
        0,
        "no grant-scoped session orphaned after the ordered restore"
    );
    // The second, independent chain: workspace→connection→mailbox→email.
    assert_eq!(
        scalar_i64(format!(
            "SELECT count(*) FROM emails em JOIN mailboxes mb ON em.mailbox_id = mb.id \
             JOIN connections c ON mb.connection_id = c.id JOIN workspaces w ON c.workspace_id = w.id \
             WHERE em.id = '{EMAIL}'"
        ))
        .await,
        1,
        "email resolves its full mailbox→connection→workspace chain"
    );
    // The diamond: mcp_endpoints depends on both workspace and grant.
    assert_eq!(
        scalar_i64(format!(
            "SELECT count(*) FROM mcp_endpoints e JOIN workspaces w ON e.workspace_id = w.id \
             JOIN grants g ON e.grant_id = g.id WHERE e.id = '{MCP}'"
        ))
        .await,
        1,
        "mcp_endpoint resolves both its workspace and grant FKs"
    );

    let _ = tokio::fs::remove_dir_all(&tmp).await;
}

/// Multi-store backup (SOUL §30): two named source stores are each mirrored under
/// `objects/<name>/`, and a restore routes each store's blobs back to the live
/// backend registered under the same name. Gated on a live Postgres; skips offline.
#[tokio::test]
async fn backup_restore_mirrors_multiple_stores() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping backup_restore_mirrors_multiple_stores: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };

    let (store, _db) = isolated_store(&url).await;

    let tmp = std::env::temp_dir().join(format!("catalerum-backup-multi-{}", uuid::Uuid::new_v4()));
    let default_store: Arc<dyn StorageBackend> = Arc::new(LocalFsBackend::new(tmp.join("default")));
    let media_store: Arc<dyn StorageBackend> = Arc::new(LocalFsBackend::new(tmp.join("media")));
    let dest: Arc<dyn StorageBackend> = Arc::new(LocalFsBackend::new(tmp.join("dest")));

    // A distinct blob in each store (same key name, different store + bytes).
    put_blob(default_store.as_ref(), "doc.txt", b"in the default store").await;
    put_blob(media_store.as_ref(), "doc.txt", b"in the media store").await;

    let engine = BackupEngine::new(store.pool().clone(), dest.clone(), "test")
        .with_prefix("backups")
        .with_named_source("default", default_store.clone())
        .with_named_source("media", media_store.clone())
        .with_keep(1);

    let summary = engine.run().await.expect("backup run");
    assert_eq!(summary.objects, 2, "both stores' blobs are copied");
    let manifest = engine.read_manifest(&summary.id).await.expect("manifest");
    assert_eq!(manifest.format_version, catalerum_backup::FORMAT_VERSION);
    let mut names: Vec<&str> = manifest
        .objects
        .stores
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec!["default", "media"],
        "per-store inventory recorded"
    );
    assert!(manifest.objects.stores.iter().all(|s| s.count == 1));

    // Wipe both live stores.
    default_store.delete("doc.txt").await.expect("wipe default");
    media_store.delete("doc.txt").await.expect("wipe media");

    // Restore reunites each store's blob with the right backend.
    let restored = engine.restore(&summary.id, false).await.expect("restore");
    assert_eq!(restored.objects, 2, "both blobs restored");
    assert_eq!(
        read_blob(default_store.as_ref(), "doc.txt").await,
        b"in the default store",
        "default store's blob is not cross-contaminated"
    );
    assert_eq!(
        read_blob(media_store.as_ref(), "doc.txt").await,
        b"in the media store",
        "media store's blob is routed to the media backend"
    );

    // A backup whose source set is missing a store named in the manifest must
    // refuse to restore that store's blobs (rather than silently drop them).
    let partial = BackupEngine::new(store.pool().clone(), dest.clone(), "test")
        .with_prefix("backups")
        .with_named_source("default", default_store.clone());
    assert!(
        partial.restore(&summary.id, false).await.is_err(),
        "restore errors when a backed-up store has no live counterpart"
    );

    let _ = tokio::fs::remove_dir_all(&tmp).await;
}
