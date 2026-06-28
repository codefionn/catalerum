//! Integration test: the `AutomationRepo` contract (SOUL §11, §6.1/§18). Create
//! / get / get_by_name / list / idempotent upsert-by-name / set_enabled toggle /
//! delete, full JSON round-trip (triggers/condition/actions/spec) + the
//! `grant_id` reference (FK-backed by a real grant, per migration `0016`), the
//! unique-name conflict, and cross-workspace isolation on both the read and
//! destructive paths.
//!
//! Same DB gating as the other store tests: set `CATALERUM_TEST_DATABASE_URL`
//! (or `DATABASE_URL`) to run it; otherwise it skips and passes offline.

use catalerum_store::{NewAutomation, Store, StoreError};
use serde_json::json;

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

fn automation(name: &str) -> NewAutomation {
    NewAutomation {
        name: name.to_string(),
        enabled: true,
        triggers: vec![json!({ "kind": "schedule", "cron": "0 9 * * *" })],
        condition: None,
        actions: vec![json!({ "kind": "summarize" })],
        spec: None,
        grant_id: None,
    }
}

#[tokio::test]
async fn automation_crud_upsert_toggle_and_isolation() {
    let Some(url) = test_db_url() else {
        eprintln!("skipping automation_crud test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };

    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("autos", &format!("autos-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let other = store
        .workspaces()
        .create("autos-b", &format!("autos-b-{}", uuid::Uuid::new_v4()))
        .await
        .expect("other ws");

    // Create + read back by id and by name.
    let created = store
        .automations()
        .create(ws.id, &automation("daily-digest"))
        .await
        .expect("create");
    assert!(created.enabled);
    assert_eq!(created.triggers.len(), 1);
    assert_eq!(created.triggers[0]["cron"], json!("0 9 * * *"));
    assert!(created.condition.is_none());
    assert!(created.grant_id.is_none());
    assert_eq!(
        store
            .automations()
            .get(ws.id, created.id)
            .await
            .unwrap()
            .name,
        "daily-digest"
    );
    assert_eq!(
        store
            .automations()
            .get_by_name(ws.id, "daily-digest")
            .await
            .unwrap()
            .unwrap()
            .id,
        created.id
    );
    assert!(store
        .automations()
        .get_by_name(ws.id, "nope")
        .await
        .unwrap()
        .is_none());

    // Duplicate name → conflict.
    assert!(matches!(
        store
            .automations()
            .create(ws.id, &automation("daily-digest"))
            .await,
        Err(StoreError::Conflict(_))
    ));

    // Upsert-by-name replaces *every* mutable column (incl. all JSONB + grant_id)
    // through a real change, keeping the stable id. A real grant must back the id:
    // migration `0016` made `automations.grant_id` a composite FK
    // `(workspace_id, grant_id) → grants`, not a soft reference.
    let grant = store
        .grants()
        .upsert(ws.id, "digest-grant", &[], &Default::default())
        .await
        .expect("grant")
        .id;
    let mut updated = automation("daily-digest");
    updated.enabled = false;
    updated.triggers =
        vec![json!({ "kind": "task_moved", "board": "sprint", "to_column": "done" })];
    updated.condition = Some(json!({ "predicate": "has_tag", "tag": "urgent" }));
    updated.actions = vec![
        json!({ "kind": "notify", "channel": "matrix" }),
        json!({ "kind": "create_note" }),
    ];
    updated.spec = Some(json!({ "source": "yaml", "raw": "name: daily-digest" }));
    updated.grant_id = Some(grant);
    let up = store
        .automations()
        .upsert_by_name(ws.id, &updated)
        .await
        .expect("upsert");
    assert_eq!(up.id, created.id, "upsert keeps the stable id");
    assert!(!up.enabled);
    assert_eq!(up.triggers[0]["to_column"], json!("done"));
    assert_eq!(up.condition.as_ref().unwrap()["tag"], json!("urgent"));
    assert_eq!(up.actions.len(), 2);
    assert_eq!(up.spec.as_ref().unwrap()["source"], json!("yaml"));
    assert_eq!(
        up.grant_id,
        Some(grant),
        "grant_id round-trips through JSONB-adjacent UUID column"
    );
    assert_eq!(
        store
            .automations()
            .list_by_workspace(ws.id)
            .await
            .unwrap()
            .len(),
        1
    );

    // A second upsert clears the nullable condition/spec/grant back to None.
    let mut cleared = updated.clone();
    cleared.condition = None;
    cleared.spec = None;
    cleared.grant_id = None;
    let up2 = store
        .automations()
        .upsert_by_name(ws.id, &cleared)
        .await
        .expect("upsert clear");
    assert_eq!(up2.id, created.id);
    assert!(up2.condition.is_none() && up2.spec.is_none() && up2.grant_id.is_none());

    // The no-conflict branch of upsert_by_name (a fresh name) inserts a new row.
    let fresh = store
        .automations()
        .upsert_by_name(ws.id, &automation("weekly-review"))
        .await
        .expect("upsert inserts a new row");
    assert_ne!(fresh.id, created.id, "a fresh name gets its own id");
    assert_eq!(
        store
            .automations()
            .get_by_name(ws.id, "weekly-review")
            .await
            .unwrap()
            .unwrap()
            .id,
        fresh.id
    );

    // set_enabled toggles `enabled` without rewriting the definition — assert
    // *both* directions so a hardcoded value can't pass. Wrong-workspace is NotFound.
    let on = store
        .automations()
        .set_enabled(ws.id, created.id, true)
        .await
        .expect("enable");
    assert!(on.enabled);
    assert_eq!(
        on.triggers[0]["to_column"],
        json!("done"),
        "toggle preserves the definition"
    );
    let off = store
        .automations()
        .set_enabled(ws.id, created.id, false)
        .await
        .expect("disable");
    assert!(
        !off.enabled,
        "set_enabled honours the false (pause) direction too"
    );
    assert!(matches!(
        store
            .automations()
            .set_enabled(other.id, created.id, true)
            .await,
        Err(StoreError::NotFound)
    ));

    // Delete the fresh row; gone afterwards, and a re-delete is NotFound.
    store.automations().delete(ws.id, fresh.id).await.unwrap();
    assert!(matches!(
        store.automations().get(ws.id, fresh.id).await,
        Err(StoreError::NotFound)
    ));
    assert!(matches!(
        store.automations().delete(ws.id, fresh.id).await,
        Err(StoreError::NotFound)
    ));

    // Cross-workspace isolation — reads and the destructive path. `created` still
    // lives in `ws`.
    assert!(store
        .automations()
        .list_by_workspace(other.id)
        .await
        .unwrap()
        .is_empty());
    assert!(store
        .automations()
        .get_by_name(other.id, "daily-digest")
        .await
        .unwrap()
        .is_none());
    assert!(matches!(
        store.automations().get(other.id, created.id).await,
        Err(StoreError::NotFound)
    ));
    assert!(matches!(
        store.automations().delete(other.id, created.id).await,
        Err(StoreError::NotFound)
    ));
    assert!(
        store.automations().get(ws.id, created.id).await.is_ok(),
        "wrong-workspace delete left the row intact"
    );
}
