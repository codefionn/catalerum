//! Integration test: the per-workspace sandbox record on `workspace_sandboxes`
//! (SOUL §20). Exactly one row per workspace (the PK is `workspace_id`); upsert
//! refreshes it, `set_status`/`touch_activity` mutate it, `mark_all_stopped` is
//! the boot reconcile, and tenancy is isolated across workspaces.
//!
//! Same DB gating as the other store tests: set `CATALERUM_TEST_DATABASE_URL`
//! (or `DATABASE_URL`) to run it; otherwise it skips and passes offline.

use catalerum_core::model::{ExecutorKind, SandboxState};
use catalerum_store::{NewWorkspaceSandbox, Store};

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

fn new_sandbox(image: &str) -> NewWorkspaceSandbox {
    new_sandbox_owned(image, None)
}

fn new_sandbox_owned(image: &str, pod_id: Option<&str>) -> NewWorkspaceSandbox {
    NewWorkspaceSandbox {
        backend: ExecutorKind::Container,
        image: image.to_string(),
        status: SandboxState::Ready,
        container_ref: Some("catalerum-ws-x".to_string()),
        volume_ref: Some("catalerum-ws-x-work".to_string()),
        pod_id: pod_id.map(str::to_string),
    }
}

/// Serializes the tests that call `mark_all_stopped`/`mark_all_stopped_for_pod`
/// (both touch rows across all workspaces — the pod-scoped variant also claims
/// NULL-pod rows), so libtest's concurrency can't race the reconcile assertions
/// against the shared DB.
fn reconcile_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

#[tokio::test]
async fn upsert_get_status_and_one_per_workspace() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping upsert_get_status_and_one_per_workspace: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };
    // Calls the global `mark_all_stopped`; serialize against the pod-scoped test.
    let _guard = reconcile_lock().lock().await;

    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("wsb", &format!("wsb-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");

    // No record yet.
    assert!(store
        .workspace_sandboxes()
        .get(ws.id)
        .await
        .expect("get unset")
        .is_none());

    // Upsert creates the row.
    let created = store
        .workspace_sandboxes()
        .upsert(ws.id, &new_sandbox("debian:stable-slim"))
        .await
        .expect("upsert");
    assert_eq!(created.workspace_id, ws.id);
    assert_eq!(created.status, SandboxState::Ready);
    assert_eq!(created.image, "debian:stable-slim");

    // A second upsert replaces (PK = workspace_id ⇒ exactly one row).
    let updated = store
        .workspace_sandboxes()
        .upsert(ws.id, &new_sandbox("alpine:latest"))
        .await
        .expect("re-upsert");
    assert_eq!(updated.image, "alpine:latest");
    assert_eq!(
        store
            .workspace_sandboxes()
            .list_all()
            .await
            .expect("list")
            .iter()
            .filter(|r| r.workspace_id == ws.id)
            .count(),
        1,
        "exactly one sandbox row per workspace"
    );

    // set_status + touch_activity mutate in place.
    let stopped = store
        .workspace_sandboxes()
        .set_status(ws.id, SandboxState::Stopped)
        .await
        .expect("set_status");
    assert_eq!(stopped.status, SandboxState::Stopped);
    store
        .workspace_sandboxes()
        .touch_activity(ws.id)
        .await
        .expect("touch");

    // mark_all_stopped is idempotent once already stopped.
    store
        .workspace_sandboxes()
        .upsert(ws.id, &new_sandbox("busybox"))
        .await
        .expect("re-ready");
    let n = store
        .workspace_sandboxes()
        .mark_all_stopped()
        .await
        .expect("mark_all_stopped");
    assert!(n >= 1, "the ready row is reconciled to stopped");

    // delete removes it.
    store
        .workspace_sandboxes()
        .delete(ws.id)
        .await
        .expect("delete");
    assert!(store
        .workspace_sandboxes()
        .get(ws.id)
        .await
        .expect("get after delete")
        .is_none());
}

#[tokio::test]
async fn sandboxes_are_isolated_per_workspace() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping sandboxes_are_isolated_per_workspace: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };
    // Creates a NULL-pod row; hold the reconcile lock so it can't be claimed by
    // (or perturb the count of) the pod-scoped reconcile test running in parallel.
    let _guard = reconcile_lock().lock().await;

    let store = Store::connect(&url).await.expect("connect+migrate");
    let a = store
        .workspaces()
        .create("wsb-a", &format!("wsb-a-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws a");
    let b = store
        .workspaces()
        .create("wsb-b", &format!("wsb-b-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws b");

    store
        .workspace_sandboxes()
        .upsert(a.id, &new_sandbox("image-a"))
        .await
        .expect("upsert a");
    // b has none even though a does.
    assert!(store
        .workspace_sandboxes()
        .get(b.id)
        .await
        .expect("get b")
        .is_none());
    assert_eq!(
        store
            .workspace_sandboxes()
            .get(a.id)
            .await
            .expect("get a")
            .expect("a exists")
            .image,
        "image-a"
    );
}

/// Multi-pod HA boot reconcile (SOUL §16 M7): `mark_all_stopped_for_pod` stops
/// only the calling pod's sandbox rows plus legacy NULL-pod rows, leaving a peer
/// pod's row running — so a rolling restart doesn't tear down another pod's live
/// sandbox out from under it.
#[tokio::test]
async fn mark_all_stopped_for_pod_scopes_to_owner_and_claims_null_rows() {
    let Some(url) = test_db_url() else {
        eprintln!("skipping mark_all_stopped_for_pod_scopes_to_owner_and_claims_null_rows: set CATALERUM_TEST_DATABASE_URL");
        return;
    };
    let _guard = reconcile_lock().lock().await;
    let store = Store::connect(&url).await.expect("connect+migrate");
    let sb = store.workspace_sandboxes();

    // Clear the field so the peer-pod assertion below isn't perturbed by leftovers.
    let _ = sb.mark_all_stopped().await.expect("baseline reconcile");

    let mk_ws = |slug: &str| {
        let store = store.clone();
        let slug = slug.to_string();
        async move {
            store
                .workspaces()
                .create(&slug, &format!("{slug}-{}", uuid::Uuid::new_v4()))
                .await
                .expect("ws")
        }
    };
    let ws_mine = mk_ws("sbpod-a").await;
    let ws_peer = mk_ws("sbpod-b").await;
    let ws_legacy = mk_ws("sbpod-legacy").await;

    sb.upsert(ws_mine.id, &new_sandbox_owned("img", Some("pod-A")))
        .await
        .expect("mine");
    sb.upsert(ws_peer.id, &new_sandbox_owned("img", Some("pod-B")))
        .await
        .expect("peer");
    sb.upsert(ws_legacy.id, &new_sandbox_owned("img", None))
        .await
        .expect("legacy");

    // Pod A's reconcile: stops its own row + the NULL row, leaves pod-B's Ready.
    let n = sb
        .mark_all_stopped_for_pod("pod-A")
        .await
        .expect("scoped reconcile");
    assert_eq!(n, 2, "reclaims only pod-A's row and the legacy NULL row");

    let status = |ws_id| {
        let sb = sb.clone();
        async move { sb.get(ws_id).await.expect("get").expect("present").status }
    };
    assert_eq!(status(ws_mine.id).await, SandboxState::Stopped);
    assert_eq!(status(ws_legacy.id).await, SandboxState::Stopped);
    assert_eq!(
        status(ws_peer.id).await,
        SandboxState::Ready,
        "a peer pod's live sandbox must survive another pod's reconcile"
    );

    // Idempotent for pod A; pod B reclaims its own row on its own restart.
    assert_eq!(
        sb.mark_all_stopped_for_pod("pod-A")
            .await
            .expect("idempotent"),
        0
    );
    assert_eq!(
        sb.mark_all_stopped_for_pod("pod-B")
            .await
            .expect("peer reconcile"),
        1
    );
}
