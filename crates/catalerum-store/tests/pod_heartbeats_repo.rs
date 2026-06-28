//! Integration test: the `PodHeartbeatRepo` contract + the stale-pod reclaim
//! sweeps (pod-heartbeat follow-up, SOUL §20/§16 M7).
//!
//! Covers heartbeat upsert, prune, stale-pod reclaim of terminal/sandbox rows,
//! the **never-heartbeated safety rule** (a pod with no heartbeat row is left
//! alone), and sweep idempotence.
//!
//! Isolation: every test uses UNIQUE (uuid-suffixed) pod ids and asserts on
//! specific rows' statuses — never global counts — so the tests don't interfere
//! with each other (the reclaim UPDATE only touches rows whose pod_id has a
//! *stale heartbeat*, and each test seeds heartbeats only for its own pods).
//!
//! Same DB gating as the other store tests: set `CATALERUM_TEST_DATABASE_URL`
//! (or `DATABASE_URL`) to run it; otherwise it skips and passes offline.

use std::time::Duration;

use catalerum_core::model::{ExecutorKind, SandboxState, TerminalSessionStatus};
use catalerum_store::{NewTerminalSession, NewWorkspaceSandbox, Store};

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

/// A unique pod id per call so concurrent tests never share reclaim state.
fn pod(tag: &str) -> String {
    format!("pod-{tag}-{}", uuid::Uuid::new_v4())
}

/// Seed a heartbeat row for `pod_id` whose `last_seen` is `age_secs` in the past.
/// Uses the pool directly so tests can inject a controlled clock (age 0 = fresh;
/// a large age = stale). Idempotent per pod (upsert).
async fn seed_heartbeat(store: &Store, pod_id: &str, age_secs: f64) {
    sqlx::query(
        "INSERT INTO pod_heartbeats (pod_id, last_seen) \
         VALUES ($1, now() - make_interval(secs => $2)) \
         ON CONFLICT (pod_id) DO UPDATE SET last_seen = EXCLUDED.last_seen",
    )
    .bind(pod_id)
    .bind(age_secs)
    .execute(store.pool())
    .await
    .expect("seed heartbeat");
}

async fn active_terminal(
    store: &Store,
    ws: catalerum_core::WorkspaceId,
    owner: Option<&str>,
) -> catalerum_core::TerminalSessionId {
    store
        .terminal_sessions()
        .create(
            ws,
            &NewTerminalSession {
                backend: ExecutorKind::Local,
                host_dir: Some("/tmp/hb".into()),
                sync_prefix: None,
                pod_id: owner.map(str::to_string),
            },
        )
        .await
        .expect("create session")
        .id
}

async fn terminal_status(
    store: &Store,
    ws: catalerum_core::WorkspaceId,
    id: catalerum_core::TerminalSessionId,
) -> TerminalSessionStatus {
    store
        .terminal_sessions()
        .get(ws, id)
        .await
        .expect("get")
        .expect("present")
        .status
}

#[tokio::test]
async fn heartbeat_upsert_inserts_then_refreshes_last_seen() {
    let Some(url) = test_db_url() else {
        eprintln!("skipping heartbeat_upsert_inserts_then_refreshes_last_seen: set CATALERUM_TEST_DATABASE_URL");
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let hb = store.pod_heartbeats();
    let me = pod("hb");

    // No row yet.
    assert!(hb.last_seen(&me).await.expect("last_seen").is_none());

    // Seed a stale row, then a live heartbeat must move last_seen forward (the
    // ON CONFLICT update path).
    seed_heartbeat(&store, &me, 3600.0).await;
    let stale = hb.last_seen(&me).await.expect("stale ts").expect("present");
    hb.heartbeat(&me).await.expect("heartbeat");
    let fresh = hb.last_seen(&me).await.expect("fresh ts").expect("present");
    assert!(
        fresh > stale,
        "heartbeat upsert must refresh last_seen (fresh={fresh}, stale={stale})"
    );

    // A first-time heartbeat (insert path) also records a row.
    let other = pod("hb-insert");
    hb.heartbeat(&other).await.expect("first heartbeat");
    assert!(hb.last_seen(&other).await.expect("last_seen").is_some());
}

#[tokio::test]
async fn reclaim_closes_only_provably_dead_pods_rows() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping reclaim_closes_only_provably_dead_pods_rows: set CATALERUM_TEST_DATABASE_URL"
        );
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let grace = Duration::from_secs(5 * 60);

    let dead = pod("dead");
    let live = pod("live");
    let never = pod("never");
    // dead pod: heartbeated but gone stale (1h > 5m grace) → provably dead.
    seed_heartbeat(&store, &dead, 3600.0).await;
    // live pod: fresh heartbeat → alive, keeps its rows.
    seed_heartbeat(&store, &live, 0.0).await;
    // `never`: NO heartbeat row at all (pre-heartbeat-code / just-booted pod).

    // --- terminal sessions ---
    let ws = store
        .workspaces()
        .create("hb-term", &format!("hb-term-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let s_dead = active_terminal(&store, ws.id, Some(&dead)).await;
    let s_live = active_terminal(&store, ws.id, Some(&live)).await;
    let s_never = active_terminal(&store, ws.id, Some(&never)).await;
    let s_null = active_terminal(&store, ws.id, None).await;

    let closed = store
        .terminal_sessions()
        .reclaim_stale_for_dead_pods(grace)
        .await
        .expect("terminal reclaim");
    assert!(
        closed >= 1,
        "at least this pod's own dead session is reclaimed"
    );

    assert_eq!(
        terminal_status(&store, ws.id, s_dead).await,
        TerminalSessionStatus::Closed,
        "a provably-dead pod's session is reclaimed"
    );
    assert_eq!(
        terminal_status(&store, ws.id, s_live).await,
        TerminalSessionStatus::Active,
        "a live (fresh-heartbeat) pod's session survives"
    );
    assert_eq!(
        terminal_status(&store, ws.id, s_never).await,
        TerminalSessionStatus::Active,
        "never-heartbeated safety rule: a pod with no heartbeat row is NOT reclaimed"
    );
    assert_eq!(
        terminal_status(&store, ws.id, s_null).await,
        TerminalSessionStatus::Active,
        "a NULL-pod row is left to the legacy boot-reconcile path, not the sweep"
    );

    // --- workspace sandboxes (one row per workspace, so use distinct workspaces) ---
    let mk_ws = |tag: &str| {
        let store = store.clone();
        let tag = tag.to_string();
        async move {
            store
                .workspaces()
                .create(
                    &format!("hb-{tag}"),
                    &format!("hb-{tag}-{}", uuid::Uuid::new_v4()),
                )
                .await
                .expect("ws")
                .id
        }
    };
    let mk_sandbox = |ws: catalerum_core::WorkspaceId, owner: Option<&str>| {
        let store = store.clone();
        let owner = owner.map(str::to_string);
        async move {
            store
                .workspace_sandboxes()
                .upsert(
                    ws,
                    &NewWorkspaceSandbox {
                        backend: ExecutorKind::Container,
                        image: "img".into(),
                        status: SandboxState::Ready,
                        container_ref: Some("c".into()),
                        volume_ref: None,
                        pod_id: owner,
                    },
                )
                .await
                .expect("upsert sandbox");
        }
    };
    let ws_dead = mk_ws("sbx-dead").await;
    let ws_live = mk_ws("sbx-live").await;
    let ws_never = mk_ws("sbx-never").await;
    mk_sandbox(ws_dead, Some(&dead)).await;
    mk_sandbox(ws_live, Some(&live)).await;
    mk_sandbox(ws_never, Some(&never)).await;

    let stopped = store
        .workspace_sandboxes()
        .reclaim_stale_for_dead_pods(grace)
        .await
        .expect("sandbox reclaim");
    assert!(
        stopped >= 1,
        "at least this pod's own dead sandbox is reclaimed"
    );

    let sandbox_status = |ws: catalerum_core::WorkspaceId| {
        let store = store.clone();
        async move {
            store
                .workspace_sandboxes()
                .get(ws)
                .await
                .expect("get")
                .expect("present")
                .status
        }
    };
    assert_eq!(
        sandbox_status(ws_dead).await,
        SandboxState::Stopped,
        "dead pod's sandbox reclaimed"
    );
    assert_eq!(
        sandbox_status(ws_live).await,
        SandboxState::Ready,
        "live pod's sandbox survives"
    );
    assert_eq!(
        sandbox_status(ws_never).await,
        SandboxState::Ready,
        "never-heartbeated safety rule applies to sandboxes too"
    );

    // --- idempotence: a second sweep (as any pod would run it) leaves the already
    //     reclaimed rows unchanged and never errors, and never touches the survivors.
    store
        .terminal_sessions()
        .reclaim_stale_for_dead_pods(grace)
        .await
        .expect("terminal reclaim (2nd)");
    store
        .workspace_sandboxes()
        .reclaim_stale_for_dead_pods(grace)
        .await
        .expect("sandbox reclaim (2nd)");
    assert_eq!(
        terminal_status(&store, ws.id, s_dead).await,
        TerminalSessionStatus::Closed
    );
    assert_eq!(
        terminal_status(&store, ws.id, s_live).await,
        TerminalSessionStatus::Active
    );
    assert_eq!(sandbox_status(ws_dead).await, SandboxState::Stopped);
    assert_eq!(sandbox_status(ws_live).await, SandboxState::Ready);
}

#[tokio::test]
async fn reclaim_respects_grace_window_for_a_paused_but_alive_pod() {
    let Some(url) = test_db_url() else {
        eprintln!("skipping reclaim_respects_grace_window_for_a_paused_but_alive_pod: set CATALERUM_TEST_DATABASE_URL");
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    // A pod last seen 60s ago with a 5-min grace is NOT yet stale (a brief pause).
    let paused = pod("paused");
    seed_heartbeat(&store, &paused, 60.0).await;
    let ws = store
        .workspaces()
        .create("hb-grace", &format!("hb-grace-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let s = active_terminal(&store, ws.id, Some(&paused)).await;

    store
        .terminal_sessions()
        .reclaim_stale_for_dead_pods(Duration::from_secs(5 * 60))
        .await
        .expect("reclaim");
    assert_eq!(
        terminal_status(&store, ws.id, s).await,
        TerminalSessionStatus::Active,
        "a pod within the grace window keeps its rows"
    );

    // With a tighter grace (30s), the same 60s-old heartbeat is now stale.
    store
        .terminal_sessions()
        .reclaim_stale_for_dead_pods(Duration::from_secs(30))
        .await
        .expect("reclaim tight");
    assert_eq!(
        terminal_status(&store, ws.id, s).await,
        TerminalSessionStatus::Closed,
        "once past the grace window the row is reclaimed"
    );
}

#[tokio::test]
async fn prune_removes_only_ancient_heartbeats() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping prune_removes_only_ancient_heartbeats: set CATALERUM_TEST_DATABASE_URL"
        );
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let hb = store.pod_heartbeats();

    let ancient = pod("ancient");
    let recent = pod("recent");
    seed_heartbeat(&store, &ancient, 8.0 * 24.0 * 60.0 * 60.0).await; // 8 days
    seed_heartbeat(&store, &recent, 60.0 * 60.0).await; // 1 hour

    hb.prune(Duration::from_secs(7 * 24 * 60 * 60))
        .await
        .expect("prune");

    assert!(
        hb.last_seen(&ancient).await.expect("ancient").is_none(),
        "a heartbeat older than the horizon is pruned"
    );
    assert!(
        hb.last_seen(&recent).await.expect("recent").is_some(),
        "a recent heartbeat is kept"
    );
}
