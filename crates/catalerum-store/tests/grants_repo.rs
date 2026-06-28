//! Integration test: `GrantRepo` CRUD + `(workspace_id, name)` idempotency + §18
//! isolation, plus the `automations.grant_id` FK detaching on grant delete (§19).
//!
//! DB-gated like the other store tests: set `CATALERUM_TEST_DATABASE_URL` (or
//! `DATABASE_URL`) to run it; otherwise it skips and passes offline.

use catalerum_core::capability::{Action, Capability, Constraints, Resource};
use catalerum_store::{NewAutomation, Store};
use serde_json::json;

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

#[tokio::test]
async fn grant_crud_is_idempotent_and_workspace_scoped() {
    let Some(url) = test_db_url() else {
        eprintln!("skipping grant_crud_is_idempotent_and_workspace_scoped: set CATALERUM_TEST_DATABASE_URL");
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("g", &format!("g-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let other = store
        .workspaces()
        .create("g2", &format!("g2-{}", uuid::Uuid::new_v4()))
        .await
        .expect("other ws");

    // A grant conferring `tasks:delete` — a capability base-Member does NOT hold.
    let caps = vec![Capability::new(Action::Delete, Resource::domain("tasks"))];
    let g = store
        .grants()
        .upsert(ws.id, "triage-powers", &caps, &Constraints::default())
        .await
        .expect("create grant");
    assert_eq!(g.name, "triage-powers");
    assert_eq!(
        g.capabilities, caps,
        "capabilities round-trip through JSONB"
    );

    // get fetches it back, scoped to the workspace.
    let fetched = store.grants().get(ws.id, g.id).await.expect("get");
    assert_eq!(fetched.id, g.id);
    assert_eq!(fetched.capabilities, caps);

    // upsert by name is idempotent: same id, refreshed capabilities, no duplicate.
    let caps2 = vec![Capability::new(Action::Run, Resource::domain("exec"))];
    let g2 = store
        .grants()
        .upsert(ws.id, "triage-powers", &caps2, &Constraints::default())
        .await
        .expect("re-upsert");
    assert_eq!(g2.id, g.id, "re-defining a named grant keeps its id");
    assert_eq!(g2.capabilities, caps2, "capabilities refreshed");
    assert_eq!(
        store.grants().list_by_workspace(ws.id).await.unwrap().len(),
        1,
        "no duplicate row"
    );

    // §18: another workspace can neither fetch nor list it.
    assert!(
        store.grants().get(other.id, g.id).await.is_err(),
        "cross-workspace get denied"
    );
    assert!(store
        .grants()
        .list_by_workspace(other.id)
        .await
        .unwrap()
        .is_empty());

    // delete is idempotent (true, then false), and the grant is then gone.
    assert!(store.grants().delete(ws.id, g.id).await.unwrap());
    assert!(
        !store.grants().delete(ws.id, g.id).await.unwrap(),
        "second delete is a no-op"
    );
    assert!(store.grants().get(ws.id, g.id).await.is_err());
}

#[tokio::test]
async fn deleting_a_grant_detaches_referencing_automations() {
    let Some(url) = test_db_url() else {
        eprintln!("skipping deleting_a_grant_detaches_referencing_automations: set CATALERUM_TEST_DATABASE_URL");
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("gfk", &format!("gfk-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");

    let g = store
        .grants()
        .upsert(
            ws.id,
            "powers",
            &[Capability::new(Action::Delete, Resource::domain("tasks"))],
            &Constraints::default(),
        )
        .await
        .expect("grant");

    // An automation that runs under the grant (grant_id set via the store; the REST
    // create path intentionally omits it — the policy engine assigns it).
    let auto = store
        .automations()
        .create(
            ws.id,
            &NewAutomation {
                name: "watcher".into(),
                enabled: true,
                triggers: vec![json!({ "kind": "webhook", "path": "/x" })],
                condition: None,
                actions: vec![json!({ "kind": "summarize" })],
                spec: None,
                grant_id: Some(g.id),
            },
        )
        .await
        .expect("automation");
    assert_eq!(
        auto.grant_id,
        Some(g.id),
        "the automation references the grant"
    );

    // Deleting the grant detaches the automation (FK ON DELETE SET NULL) rather than
    // dangling — the automation falls back to its default authority.
    assert!(store.grants().delete(ws.id, g.id).await.unwrap());
    let detached = store
        .automations()
        .list_by_workspace(ws.id)
        .await
        .unwrap()
        .into_iter()
        .find(|a| a.id == auto.id)
        .expect("automation still exists");
    assert_eq!(
        detached.grant_id, None,
        "the FK nulled grant_id on grant delete"
    );
}

#[tokio::test]
async fn an_automation_cannot_reference_a_foreign_workspace_grant() {
    let Some(url) = test_db_url() else {
        eprintln!("skipping an_automation_cannot_reference_a_foreign_workspace_grant: set CATALERUM_TEST_DATABASE_URL");
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws_a = store
        .workspaces()
        .create("wsa", &format!("wsa-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws a");
    let ws_b = store
        .workspaces()
        .create("wsb", &format!("wsb-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws b");

    // A grant in workspace A.
    let grant_a = store
        .grants()
        .upsert(ws_a.id, "a-powers", &[], &Constraints::default())
        .await
        .expect("grant a");

    // Creating a workspace-B automation that references A's grant is rejected by the
    // DB (the composite `(workspace_id, grant_id)` FK enforces same-workspace, §18).
    let cross = store
        .automations()
        .create(
            ws_b.id,
            &NewAutomation {
                name: "cross-tenant".into(),
                enabled: true,
                triggers: vec![json!({ "kind": "webhook", "path": "/x" })],
                condition: None,
                actions: vec![json!({ "kind": "summarize" })],
                spec: None,
                grant_id: Some(grant_a.id),
            },
        )
        .await;
    assert!(
        cross.is_err(),
        "the DB must reject a cross-workspace grant reference"
    );
}
