//! Integration test: the `SkillRepo` contract (SOUL §23, §6.1/§18). Create /
//! get / get_by_name / list / idempotent upsert-by-name / delete, a code-bearing
//! skill round-trip, the unique-name conflict, and cross-workspace isolation.
//!
//! Same DB gating as the other store tests: set `CATALERUM_TEST_DATABASE_URL`
//! (or `DATABASE_URL`) to run it; otherwise it skips and passes offline.

use catalerum_core::model::Code;
use catalerum_store::{NewSkill, Store, StoreError};

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

fn skill(name: &str) -> NewSkill {
    NewSkill {
        name: name.to_string(),
        description: "desc".into(),
        instructions_md: "# runbook".into(),
        tools: vec!["read_note".into(), "kanban_create_task".into()],
        code: None,
        advertised: true,
    }
}

#[tokio::test]
async fn skill_crud_upsert_code_and_isolation() {
    let Some(url) = test_db_url() else {
        eprintln!("skipping skill_crud_upsert_code_and_isolation: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };

    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("skills", &format!("skills-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let other = store
        .workspaces()
        .create("skills-b", &format!("skills-b-{}", uuid::Uuid::new_v4()))
        .await
        .expect("other ws");

    // Create + read back by id and by name.
    let created = store
        .skills()
        .create(ws.id, &skill("summarize"))
        .await
        .expect("create");
    assert_eq!(
        created.tools,
        vec!["read_note".to_string(), "kanban_create_task".to_string()]
    );
    assert!(created.code.is_none());
    assert_eq!(
        store.skills().get(ws.id, created.id).await.unwrap().name,
        "summarize"
    );
    assert_eq!(
        store
            .skills()
            .get_by_name(ws.id, "summarize")
            .await
            .unwrap()
            .unwrap()
            .id,
        created.id
    );
    assert!(store
        .skills()
        .get_by_name(ws.id, "nope")
        .await
        .unwrap()
        .is_none());

    // Duplicate name → conflict.
    assert!(matches!(
        store.skills().create(ws.id, &skill("summarize")).await,
        Err(StoreError::Conflict(_))
    ));

    // Upsert-by-name refreshes the existing row (no duplicate), keeping the id,
    // and drives *every* column in the DO UPDATE SET clause through a real change
    // — including the riskiest one, the nullable `code` JSONB (None -> Some).
    let mut updated = skill("summarize");
    updated.description = "new desc".into();
    updated.instructions_md = "# refreshed".into();
    updated.tools = vec!["search_semantic".into()];
    updated.code = Some(Code {
        language: "python".into(),
        source: "x".into(),
        entrypoint: None,
    });
    let up = store
        .skills()
        .upsert_by_name(ws.id, &updated)
        .await
        .expect("upsert");
    assert_eq!(up.id, created.id, "upsert keeps the stable id");
    assert_eq!(up.description, "new desc");
    assert_eq!(
        up.instructions_md, "# refreshed",
        "instructions_md = EXCLUDED is applied"
    );
    assert_eq!(up.tools, vec!["search_semantic".to_string()]);
    assert_eq!(
        up.code.as_ref().unwrap().language,
        "python",
        "code = EXCLUDED adds code"
    );
    assert_eq!(
        store.skills().list_by_workspace(ws.id).await.unwrap().len(),
        1
    );

    // A second upsert clears code back to None (Some -> None), same row/id —
    // proving the SET clause overwrites code rather than leaving stale JSONB.
    let mut cleared = updated.clone();
    cleared.code = None;
    let up2 = store
        .skills()
        .upsert_by_name(ws.id, &cleared)
        .await
        .expect("upsert clear");
    assert_eq!(up2.id, created.id);
    assert!(
        up2.code.is_none(),
        "code = EXCLUDED clears code back to None"
    );

    // The no-conflict branch of upsert_by_name (a name not yet present) inserts a
    // fresh row with a new id — the path `seed_first_party` takes on a clean ws.
    let fresh = store
        .skills()
        .upsert_by_name(ws.id, &skill("fresh-name"))
        .await
        .expect("upsert inserts a new row");
    assert_eq!(fresh.name, "fresh-name");
    assert_ne!(fresh.id, created.id, "a fresh name gets its own id");
    assert_eq!(
        store
            .skills()
            .get_by_name(ws.id, "fresh-name")
            .await
            .unwrap()
            .unwrap()
            .id,
        fresh.id
    );
    store.skills().delete(ws.id, fresh.id).await.unwrap();

    // A code-bearing skill round-trips its Code.
    let code_skill = NewSkill {
        name: "run-report".into(),
        description: "runs a report".into(),
        instructions_md: String::new(),
        tools: vec![],
        code: Some(Code {
            language: "python".into(),
            source: "print('hi')".into(),
            entrypoint: Some("main".into()),
        }),
        advertised: true,
    };
    let cs = store
        .skills()
        .create(ws.id, &code_skill)
        .await
        .expect("code skill");
    let fetched = store.skills().get(ws.id, cs.id).await.unwrap();
    assert_eq!(fetched.code.as_ref().unwrap().language, "python");
    assert_eq!(
        fetched.code.as_ref().unwrap().entrypoint.as_deref(),
        Some("main")
    );

    // The advertised flag round-trips through upsert, and `list_advertised`
    // returns only flagged skills' (name, description) — the chat system
    // prompt's lean read (SOUL §23).
    let mut hidden = cleared.clone();
    hidden.advertised = false;
    let hid = store
        .skills()
        .upsert_by_name(ws.id, &hidden)
        .await
        .expect("upsert advertised=false");
    assert!(!hid.advertised, "advertised = EXCLUDED is applied");
    assert_eq!(
        store.skills().list_advertised(ws.id).await.unwrap(),
        vec![("run-report".to_string(), "runs a report".to_string())],
        "an unadvertised skill is skipped; the rest are (name, description)"
    );

    // Delete; gone afterwards.
    store.skills().delete(ws.id, created.id).await.unwrap();
    assert!(matches!(
        store.skills().get(ws.id, created.id).await,
        Err(StoreError::NotFound)
    ));
    assert!(matches!(
        store.skills().delete(ws.id, created.id).await,
        Err(StoreError::NotFound)
    ));

    // Cross-workspace isolation. `cs` (run-report) still lives in `ws`.
    assert!(store
        .skills()
        .list_by_workspace(other.id)
        .await
        .unwrap()
        .is_empty());
    assert!(store
        .skills()
        .get_by_name(other.id, "run-report")
        .await
        .unwrap()
        .is_none());
    assert!(matches!(
        store.skills().get(other.id, cs.id).await,
        Err(StoreError::NotFound)
    ));

    // A delete scoped to the wrong workspace is a no-op (NotFound) and must leave
    // the real row intact — tenant isolation on the *destructive* path (§18).
    assert!(matches!(
        store.skills().delete(other.id, cs.id).await,
        Err(StoreError::NotFound)
    ));
    assert!(
        store.skills().get(ws.id, cs.id).await.is_ok(),
        "wrong-workspace delete left the row intact"
    );
}

#[tokio::test]
async fn get_many_by_name_batches_and_is_workspace_scoped() {
    let Some(url) = test_db_url() else {
        eprintln!("skipping get_many_by_name_batches_and_is_workspace_scoped: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("sk-gm", &format!("sk-gm-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let other = store
        .workspaces()
        .create("sk-gm-b", &format!("sk-gm-b-{}", uuid::Uuid::new_v4()))
        .await
        .expect("other ws");

    store
        .skills()
        .create(ws.id, &skill("alpha"))
        .await
        .expect("a");
    store
        .skills()
        .create(ws.id, &skill("beta"))
        .await
        .expect("b");
    store
        .skills()
        .create(other.id, &skill("gamma"))
        .await
        .expect("g");

    // Present names resolve in one call; an absent name is simply omitted.
    let names = vec![
        "alpha".to_string(),
        "missing".to_string(),
        "beta".to_string(),
    ];
    let mut got: Vec<String> = store
        .skills()
        .get_many_by_name(ws.id, &names)
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.name)
        .collect();
    got.sort();
    assert_eq!(got, vec!["alpha".to_string(), "beta".to_string()]);

    // Empty input short-circuits to empty (no query).
    assert!(store
        .skills()
        .get_many_by_name(ws.id, &[])
        .await
        .unwrap()
        .is_empty());

    // Workspace-scoped: another workspace's skill is never returned, even by name.
    assert!(
        store
            .skills()
            .get_many_by_name(ws.id, &["gamma".to_string()])
            .await
            .unwrap()
            .is_empty(),
        "gamma belongs to `other`, not `ws`"
    );
}
