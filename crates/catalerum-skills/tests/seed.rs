//! Integration test: `seed_first_party` persists the first-party skills (SOUL
//! §23) idempotently and workspace-scoped — the path the binary runs on boot.
//!
//! DB-gated like the store tests: set `CATALERUM_TEST_DATABASE_URL` (or
//! `DATABASE_URL`) to run it; otherwise it skips and passes offline.

use catalerum_skills::{first_party_skills, seed_first_party};
use catalerum_store::Store;

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

#[tokio::test]
async fn seed_first_party_is_idempotent_and_workspace_scoped() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping seed_first_party test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
        );
        return;
    };

    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("seed", &format!("seed-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let other = store
        .workspaces()
        .create("seed-b", &format!("seed-b-{}", uuid::Uuid::new_v4()))
        .await
        .expect("other ws");

    let expected = first_party_skills().len();
    assert!(
        expected >= 3,
        "ships at least the three documented fixtures"
    );

    // First seed persists every fixture.
    let seeded = seed_first_party(&store, ws.id).await.expect("seed");
    assert_eq!(seeded.len(), expected);
    let listed = store.skills().list_by_workspace(ws.id).await.unwrap();
    assert_eq!(listed.len(), expected, "all fixtures persisted");

    // A known fixture round-trips by name with its tool set + runbook intact.
    let summarize = store
        .skills()
        .get_by_name(ws.id, "summarize")
        .await
        .unwrap()
        .expect("summarize seeded");
    assert!(!summarize.instructions_md.is_empty());
    assert!(summarize.tools.contains(&"read_note".to_string()));

    // Re-seeding is idempotent: no duplicates, ids stable (upsert-by-name).
    let before: Vec<_> = listed.iter().map(|s| (s.name.clone(), s.id)).collect();
    let reseeded = seed_first_party(&store, ws.id).await.expect("reseed");
    assert_eq!(reseeded.len(), expected);
    let after = store.skills().list_by_workspace(ws.id).await.unwrap();
    assert_eq!(after.len(), expected, "re-seed does not duplicate");
    for (name, id) in before {
        let now = after.iter().find(|s| s.name == name).expect("fixture kept");
        assert_eq!(now.id, id, "upsert keeps the stable id across a re-seed");
    }

    // Seeding `ws` left `other` empty — workspace-scoped (§18).
    assert!(store
        .skills()
        .list_by_workspace(other.id)
        .await
        .unwrap()
        .is_empty());
}
