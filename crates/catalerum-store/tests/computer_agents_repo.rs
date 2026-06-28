//! Integration test: the `computer_agents` repository (SOUL §19/§20) — enroll →
//! token-hash lookup (active only) → capability/last-seen refresh → list → revoke
//! (token stops authenticating, row retained) → delete, plus `(workspace, name)`
//! uniqueness and cross-workspace isolation.
//!
//! DB-gated like the other store tests: set `CATALERUM_TEST_DATABASE_URL` (or
//! `DATABASE_URL`) to run it; otherwise it skips and passes offline.

use catalerum_core::computer::{
    ComputerCapabilities, ComputerPlatform, DirGrant, DirMode, ExecPolicy, SandboxKind,
    PROTOCOL_VERSION,
};
use catalerum_store::Store;

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

#[tokio::test]
async fn computer_agent_enroll_touch_revoke_and_isolation() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping computer_agent_enroll_touch_revoke_and_isolation: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };

    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("ca", &format!("ca-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let other = store
        .workspaces()
        .create("ca2", &format!("ca2-{}", uuid::Uuid::new_v4()))
        .await
        .expect("other ws");
    let user = store
        .users()
        .create(&format!("u-{}@x.test", uuid::Uuid::new_v4()), "U", None)
        .await
        .expect("user");

    // Token hashes are globally UNIQUE — run-scope them so repeat runs on a shared
    // dev DB don't collide.
    let run = uuid::Uuid::new_v4();
    let hash = format!("hash-{run}");

    // (1) Enroll: platform/capabilities are NULL until first connect.
    let agent = store
        .computer_agents()
        .create(ws.id, user.id, "laptop", &hash)
        .await
        .expect("enroll");
    assert_eq!(agent.name, "laptop");
    assert!(agent.platform.is_none() && agent.capabilities.is_none());
    assert!(agent.is_active());

    // (2) Token-hash lookup finds the active agent.
    let found = store
        .computer_agents()
        .get_active_by_token_hash(&hash)
        .await
        .expect("lookup by hash");
    assert_eq!(found.id, agent.id);

    // (3) touch_seen refreshes capabilities + denormalised platform + last_seen.
    let caps = ComputerCapabilities {
        platform: ComputerPlatform::Linux,
        hostname: "laptop".into(),
        dirs: vec![DirGrant {
            path: "/work".into(),
            mode: DirMode::ReadWrite,
        }],
        grantable_roots: vec!["/home/me".into()],
        exec_policy: ExecPolicy::Auto,
        desktop: true,
        sandbox: SandboxKind::Landlock,
        protocol: PROTOCOL_VERSION,
        ..Default::default()
    };
    store
        .computer_agents()
        .touch_seen(agent.id, &caps)
        .await
        .expect("touch");
    let refreshed = store
        .computer_agents()
        .get(ws.id, agent.id)
        .await
        .expect("get after touch");
    assert_eq!(refreshed.platform, Some(ComputerPlatform::Linux));
    assert!(refreshed.last_seen_at.is_some());
    let stored = refreshed.capabilities.expect("caps stored");
    assert!(stored.desktop);
    assert_eq!(stored.dirs.len(), 1);
    assert!(stored.dirs[0].mode.can_write());

    // (4) List is workspace-scoped: this workspace sees its agent; the other sees none.
    assert_eq!(
        store
            .computer_agents()
            .list_by_workspace(ws.id)
            .await
            .expect("list ws")
            .len(),
        1
    );
    assert!(store
        .computer_agents()
        .list_by_workspace(other.id)
        .await
        .expect("list other")
        .is_empty());

    // (5) A duplicate (workspace, name) is rejected.
    assert!(
        store
            .computer_agents()
            .create(ws.id, user.id, "laptop", &format!("hash2-{run}"))
            .await
            .is_err(),
        "duplicate (workspace, name) must conflict"
    );

    // (6) Revoke: the token stops authenticating, but the row is retained (and
    // still visible by id / in the list for audit). Revoke is idempotent.
    store
        .computer_agents()
        .revoke(ws.id, agent.id)
        .await
        .expect("revoke");
    store
        .computer_agents()
        .revoke(ws.id, agent.id)
        .await
        .expect("revoke idempotent");
    assert!(
        store
            .computer_agents()
            .get_active_by_token_hash(&hash)
            .await
            .is_err(),
        "a revoked agent's token no longer authenticates"
    );
    let post = store
        .computer_agents()
        .get(ws.id, agent.id)
        .await
        .expect("row retained after revoke");
    assert!(!post.is_active() && post.revoked_at.is_some());

    // (7) Delete removes the row.
    store
        .computer_agents()
        .delete(ws.id, agent.id)
        .await
        .expect("delete");
    assert!(store.computer_agents().get(ws.id, agent.id).await.is_err());
}
