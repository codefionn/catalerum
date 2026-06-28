//! Integration test: the `TerminalSessionRepo` contract (SOUL §20, §6.1/§18).
//! Session create/status/sync-prefix/list-active/delete, cross-workspace
//! isolation, and boot reconcile. Terminals are always ephemeral now — there is
//! no persistent workdir concept.
//!
//! Same DB gating as the other store tests: set `CATALERUM_TEST_DATABASE_URL`
//! (or `DATABASE_URL`) to run it; otherwise it skips and passes offline.

use catalerum_core::model::{ExecutorKind, TerminalSessionStatus};
use catalerum_store::{NewTerminalSession, Store, StoreError};

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

/// Serializes the tests that read/write **active-session counts**: `close_all_active`
/// is global (not workspace-scoped), so it would otherwise race the count
/// assertions in `terminal_session_flow_with_tenancy_guards` when libtest runs the
/// two concurrently against the shared DB.
fn active_session_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

#[tokio::test]
async fn terminal_session_flow_with_tenancy_guards() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping terminal_session_flow_with_tenancy_guards: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };
    let _guard = active_session_lock().lock().await;

    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("term", &format!("term-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let other = store
        .workspaces()
        .create("term-b", &format!("term-b-{}", uuid::Uuid::new_v4()))
        .await
        .expect("other ws");

    let sessions = store.terminal_sessions();

    // Two ephemeral sessions on different backends.
    let one = sessions
        .create(
            ws.id,
            &NewTerminalSession {
                backend: ExecutorKind::Local,
                host_dir: Some("/tmp/eph-one".into()),
                sync_prefix: None,
                pod_id: None,
            },
        )
        .await
        .expect("create session one");
    assert_eq!(one.status, TerminalSessionStatus::Active);
    assert_eq!(one.backend, ExecutorKind::Local);

    let two = sessions
        .create(
            ws.id,
            &NewTerminalSession {
                backend: ExecutorKind::Sandbox,
                host_dir: Some("/tmp/eph-two".into()),
                sync_prefix: None,
                pod_id: None,
            },
        )
        .await
        .expect("create session two");

    // Both are active; the other workspace sees none of it.
    assert_eq!(sessions.list_active(ws.id).await.expect("active").len(), 2);
    assert!(sessions
        .list_active(other.id)
        .await
        .expect("other active")
        .is_empty());

    // Record a persist prefix, then close the second one.
    sessions
        .set_sync_prefix(ws.id, two.id, "runs/2026-06-25")
        .await
        .expect("set prefix");
    let closed = sessions
        .set_status(ws.id, two.id, TerminalSessionStatus::Closed)
        .await
        .expect("close");
    assert_eq!(closed.status, TerminalSessionStatus::Closed);
    assert!(closed.closed_at.is_some(), "closed_at is stamped on close");
    assert_eq!(closed.sync_prefix.as_deref(), Some("runs/2026-06-25"));

    // Only the first session is still active.
    let active = sessions.list_active(ws.id).await.expect("active2");
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].id, one.id);

    // set_status on a missing session is NotFound.
    let missing = catalerum_core::TerminalSessionId::new();
    assert!(matches!(
        sessions
            .set_status(ws.id, missing, TerminalSessionStatus::Failed)
            .await,
        Err(StoreError::NotFound)
    ));

    // Delete the first session; a second delete is NotFound.
    sessions.delete(ws.id, one.id).await.expect("delete one");
    assert!(matches!(
        sessions.delete(ws.id, one.id).await,
        Err(StoreError::NotFound)
    ));
}

/// Boot reconcile (SOUL §20): `close_all_active` closes every active session
/// across **all** workspaces (a restart orphans every in-memory PTY/container/Pod
/// handle), leaves already-closed rows untouched, and is idempotent.
#[tokio::test]
async fn close_all_active_reconciles_orphaned_sessions_across_workspaces() {
    let Some(url) = test_db_url() else {
        eprintln!("skipping close_all_active_reconciles_orphaned_sessions_across_workspaces: set CATALERUM_TEST_DATABASE_URL");
        return;
    };
    let _guard = active_session_lock().lock().await;
    let store = Store::connect(&url).await.expect("connect+migrate");
    let sessions = store.terminal_sessions();

    // Baseline: clear any active rows left by earlier runs so the count is exact.
    let _ = sessions
        .close_all_active()
        .await
        .expect("baseline reconcile");

    // Two workspaces, each with an active session.
    let ws_a = store
        .workspaces()
        .create("recon-a", &format!("recon-a-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws a");
    let ws_b = store
        .workspaces()
        .create("recon-b", &format!("recon-b-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws b");
    let mk = || NewTerminalSession {
        backend: ExecutorKind::Sandbox,
        host_dir: Some("/tmp/recon".into()),
        sync_prefix: None,
        pod_id: None,
    };
    let a = sessions.create(ws_a.id, &mk()).await.expect("a session");
    let _b = sessions.create(ws_b.id, &mk()).await.expect("b session");

    // A pre-closed session must not be re-stamped.
    let pre_closed = sessions.create(ws_a.id, &mk()).await.expect("pre-closed");
    let pre_closed = sessions
        .set_status(ws_a.id, pre_closed.id, TerminalSessionStatus::Closed)
        .await
        .expect("close one ahead of time");
    let original_closed_at = pre_closed.closed_at;

    // Reconcile: exactly the two active sessions flip to closed.
    let n = sessions.close_all_active().await.expect("reconcile");
    assert_eq!(n, 2, "closes only the two active sessions");

    // Both workspaces now report zero active sessions.
    assert!(sessions
        .list_active(ws_a.id)
        .await
        .expect("a active")
        .is_empty());
    assert!(sessions
        .list_active(ws_b.id)
        .await
        .expect("b active")
        .is_empty());

    // The reconciled session is closed with a fresh closed_at.
    let a_after = sessions
        .get(ws_a.id, a.id)
        .await
        .expect("get a")
        .expect("present");
    assert_eq!(a_after.status, TerminalSessionStatus::Closed);
    assert!(
        a_after.closed_at.is_some(),
        "closed_at stamped on reconcile"
    );

    // The already-closed session kept its original closed_at (not re-stamped).
    let pc_after = sessions
        .get(ws_a.id, pre_closed.id)
        .await
        .expect("get pc")
        .expect("present");
    assert_eq!(
        pc_after.closed_at, original_closed_at,
        "pre-closed row untouched"
    );

    // Idempotent: a second pass closes nothing.
    assert_eq!(sessions.close_all_active().await.expect("idempotent"), 0);
}

/// Multi-pod HA boot reconcile (SOUL §16 M7): `close_all_active_for_pod` reclaims
/// only the calling pod's rows **plus** legacy NULL-pod rows, and leaves a peer
/// pod's live sessions untouched — so a rolling restart of pod B never marks pod
/// A's healthy sessions closed.
#[tokio::test]
async fn close_all_active_for_pod_scopes_to_owner_and_claims_null_rows() {
    let Some(url) = test_db_url() else {
        eprintln!("skipping close_all_active_for_pod_scopes_to_owner_and_claims_null_rows: set CATALERUM_TEST_DATABASE_URL");
        return;
    };
    // Shares the active-count lock: NULL-row claiming would otherwise race the
    // other reconcile tests' rows across the shared DB.
    let _guard = active_session_lock().lock().await;
    let store = Store::connect(&url).await.expect("connect+migrate");
    let sessions = store.terminal_sessions();

    // Baseline: clear any active rows so the scoped counts below are exact.
    let _ = sessions
        .close_all_active()
        .await
        .expect("baseline reconcile");

    let ws = store
        .workspaces()
        .create("recon-pod", &format!("recon-pod-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let mk = |pod: Option<&str>| NewTerminalSession {
        backend: ExecutorKind::Sandbox,
        host_dir: Some("/tmp/recon-pod".into()),
        sync_prefix: None,
        pod_id: pod.map(str::to_string),
    };
    // A session owned by this pod, one owned by a peer pod, and a legacy NULL row.
    let mine = sessions
        .create(ws.id, &mk(Some("pod-A")))
        .await
        .expect("mine");
    let peer = sessions
        .create(ws.id, &mk(Some("pod-B")))
        .await
        .expect("peer");
    let legacy = sessions.create(ws.id, &mk(None)).await.expect("legacy");

    // Pod A's boot reconcile: closes exactly its own row + the NULL row (2), not peer's.
    let n = sessions
        .close_all_active_for_pod("pod-A")
        .await
        .expect("scoped reconcile");
    assert_eq!(n, 2, "reclaims only pod-A's row and the legacy NULL row");

    let get = |id| {
        let sessions = sessions.clone();
        async move {
            sessions
                .get(ws.id, id)
                .await
                .expect("get")
                .expect("present")
        }
    };
    assert_eq!(get(mine.id).await.status, TerminalSessionStatus::Closed);
    assert_eq!(get(legacy.id).await.status, TerminalSessionStatus::Closed);
    assert_eq!(
        get(peer.id).await.status,
        TerminalSessionStatus::Active,
        "a peer pod's live session must survive another pod's reconcile"
    );
    // The workspace still lists the peer's session as active.
    let active = sessions.list_active(ws.id).await.expect("active");
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].id, peer.id);

    // Idempotent for pod A; the peer pod reclaims its own row on its own restart.
    assert_eq!(
        sessions
            .close_all_active_for_pod("pod-A")
            .await
            .expect("idempotent"),
        0
    );
    assert_eq!(
        sessions
            .close_all_active_for_pod("pod-B")
            .await
            .expect("peer reconcile"),
        1,
        "pod-B reclaims its own row when it restarts"
    );
}
