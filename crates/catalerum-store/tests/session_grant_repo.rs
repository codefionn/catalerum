//! Integration test: a `sessions` row may be **scoped** to a §19 grant (SOUL
//! §19/§26) — the `grant_id` column, its round-trip through the repo, the
//! same-workspace composite FK, and `ON DELETE CASCADE` revoking a grant-bound
//! token when its grant is removed (while a grantless token survives).
//!
//! DB-gated like the other store tests: set `CATALERUM_TEST_DATABASE_URL` (or
//! `DATABASE_URL`) to run it; otherwise it skips and passes offline.

use chrono::{Duration, Utc};

use catalerum_core::capability::{Action, Capability, Constraints, Resource};
use catalerum_store::Store;

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

#[tokio::test]
async fn session_grant_round_trips_and_fails_closed_on_delete() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping session_grant_round_trips_and_fails_closed_on_delete: set CATALERUM_TEST_DATABASE_URL"
        );
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("s", &format!("s-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let other = store
        .workspaces()
        .create("s2", &format!("s2-{}", uuid::Uuid::new_v4()))
        .await
        .expect("other ws");
    let user = store
        .users()
        .create(&format!("u-{}@x.test", uuid::Uuid::new_v4()), "U", None)
        .await
        .expect("user");

    let caps = vec![Capability::new(Action::Write, Resource::domain("notes"))];
    let grant = store
        .grants()
        .upsert(ws.id, "notes-writer", &caps, &Constraints::default())
        .await
        .expect("grant");
    let other_grant = store
        .grants()
        .upsert(other.id, "elsewhere", &caps, &Constraints::default())
        .await
        .expect("other grant");

    let exp = Utc::now() + Duration::days(1);

    // Token hashes are globally unique (`sessions_token_hash_key`), and this test
    // runs against a shared dev DB — a fixed hash would collide with a prior
    // run's leftover row, so every hash is run-scoped.
    let run = uuid::Uuid::new_v4();
    let hash_grant = format!("hash-grant-{run}");
    let hash_plain = format!("hash-plain-{run}");
    let hash_cross = format!("hash-cross-{run}");

    // (1) A grant-scoped session round-trips its grant_id through create + reads.
    let bound = store
        .sessions()
        .create(ws.id, user.id, &hash_grant, Some(grant.id), exp)
        .await
        .expect("create grant-bound session");
    assert_eq!(bound.grant_id(), Some(grant.id));
    let by_hash = store
        .sessions()
        .get_by_token_hash(&hash_grant)
        .await
        .expect("get by hash");
    assert_eq!(by_hash.grant_id(), Some(grant.id));
    assert!(store
        .sessions()
        .list_by_user(user.id)
        .await
        .expect("list")
        .iter()
        .any(|s| s.grant_id() == Some(grant.id)));

    // (2) A grantless session persists a NULL grant_id.
    store
        .sessions()
        .create(ws.id, user.id, &hash_plain, None, exp)
        .await
        .expect("create plain session");
    assert_eq!(
        store
            .sessions()
            .get_by_token_hash(&hash_plain)
            .await
            .expect("get plain")
            .grant_id(),
        None
    );

    // (3) The same-workspace composite FK rejects binding a grant from ANOTHER
    // workspace (§18 defense-in-depth) — a scoped token can only reference a grant
    // in its own workspace.
    assert!(
        store
            .sessions()
            .create(ws.id, user.id, &hash_cross, Some(other_grant.id), exp)
            .await
            .is_err(),
        "a cross-workspace grant binding must be rejected by the composite FK"
    );

    // (4) Deleting the grant cascade-revokes the grant-bound token (fail closed),
    // while the grantless token survives.
    assert!(store
        .grants()
        .delete(ws.id, grant.id)
        .await
        .expect("delete grant"));
    assert!(
        store
            .sessions()
            .get_by_token_hash(&hash_grant)
            .await
            .is_err(),
        "the grant-bound session is cascade-deleted when its grant is removed"
    );
    assert!(
        store
            .sessions()
            .get_by_token_hash(&hash_plain)
            .await
            .is_ok(),
        "the grantless session is unaffected by the grant delete"
    );
}
