//! Integration test: `ConnectionRepo::ensure` — the race-free get-or-create that
//! the storage catalogue relies on (SOUL §6.1/§3.4/§18). Verifies idempotency on
//! `(workspace_id, kind, name)`, convergence with a prior `create`, that a
//! duplicate `create` is a `Conflict`, that a re-ensure preserves a stored
//! `credential_ref`, and cross-workspace independence.
//!
//! Same DB gating as the other store tests: set `CATALERUM_TEST_DATABASE_URL`
//! (or `DATABASE_URL`) to run it; otherwise it skips and passes offline.

use catalerum_core::model::ConnectionKind;
use catalerum_store::{Store, StoreError};

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

#[tokio::test]
async fn connection_ensure_is_idempotent_conflict_safe_and_isolated() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping connection_ensure_is_idempotent_conflict_safe_and_isolated: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };

    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("conn", &format!("conn-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let other = store
        .workspaces()
        .create("conn-b", &format!("conn-b-{}", uuid::Uuid::new_v4()))
        .await
        .expect("other ws");

    // ensure twice for the same (ws, kind, name) → the SAME row id (idempotent,
    // the property the concurrent-upload race needs).
    let c1 = store
        .connections()
        .ensure(ws.id, ConnectionKind::Storage, "local-storage", None, None)
        .await
        .expect("ensure 1");
    let c2 = store
        .connections()
        .ensure(ws.id, ConnectionKind::Storage, "local-storage", None, None)
        .await
        .expect("ensure 2");
    assert_eq!(c1.id, c2.id, "ensure converges on one connection");
    assert_eq!(
        store
            .connections()
            .list_by_workspace(ws.id)
            .await
            .unwrap()
            .len(),
        1,
        "no duplicate connection row"
    );

    // A direct `create` of the same (ws, kind, name) is now a Conflict (the
    // UNIQUE constraint), not a silent duplicate.
    let dup = store
        .connections()
        .create(ws.id, ConnectionKind::Storage, "local-storage", None, None)
        .await;
    assert!(
        matches!(dup, Err(StoreError::Conflict(_))),
        "duplicate create is a Conflict, got {dup:?}"
    );

    // ensure converges onto a row made by `create` (different name to avoid the
    // conflict above): create first, then ensure returns the same id.
    let made = store
        .connections()
        .create(
            ws.id,
            ConnectionKind::Calendar,
            "cal",
            Some("secret-ref"),
            None,
        )
        .await
        .expect("create cal");
    let ensured = store
        .connections()
        .ensure(ws.id, ConnectionKind::Calendar, "cal", None, None)
        .await
        .expect("ensure existing");
    assert_eq!(ensured.id, made.id, "ensure adopts the existing row");
    // A re-ensure with no credential must NOT clobber the stored secret ref.
    assert_eq!(
        ensured.credential_ref.as_deref(),
        Some("secret-ref"),
        "ensure preserves an existing credential_ref"
    );

    // §18: the same (kind, name) in another workspace is an independent row.
    let foreign = store
        .connections()
        .ensure(
            other.id,
            ConnectionKind::Storage,
            "local-storage",
            None,
            None,
        )
        .await
        .expect("ensure foreign ws");
    assert_ne!(foreign.id, c1.id, "connections are per-workspace");
    assert_eq!(
        store
            .connections()
            .list_by_workspace(other.id)
            .await
            .unwrap()
            .len(),
        1
    );
}

/// `ensure`'s `config` semantics (SOUL §9/§3.4): a re-ensure with a NEW config
/// **refreshes** the stored blob (so the email driver can move a Maildir's `root`
/// across boots), while a re-ensure with `None` **preserves** it (so the storage
/// catalogue's existence-only ensure never wipes a connection's settings). Skips
/// offline like its sibling.
#[tokio::test]
async fn connection_ensure_refreshes_config_when_given_preserves_when_omitted() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping connection_ensure_refreshes_config_when_given_preserves_when_omitted: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };

    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("cfg", &format!("cfg-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");

    // First ensure plants config A.
    let a = store
        .connections()
        .ensure(
            ws.id,
            ConnectionKind::Email,
            "mbox",
            None,
            Some(serde_json::json!({"root": "/mail/a"})),
        )
        .await
        .expect("ensure A");
    let cfg_a = store.connections().get_row(ws.id, a.id).await.unwrap();
    assert_eq!(cfg_a.config()["root"], "/mail/a");

    // Re-ensure with config B → the SAME row, with REFRESHED config (the bug fix:
    // a changed Maildir root must take effect on the next boot, not be ignored).
    let b = store
        .connections()
        .ensure(
            ws.id,
            ConnectionKind::Email,
            "mbox",
            None,
            Some(serde_json::json!({"root": "/mail/b"})),
        )
        .await
        .expect("ensure B");
    assert_eq!(a.id, b.id, "ensure converges on one row");
    let cfg_b = store.connections().get_row(ws.id, b.id).await.unwrap();
    assert_eq!(
        cfg_b.config()["root"],
        "/mail/b",
        "config is refreshed on re-ensure with Some"
    );

    // Re-ensure with None → the SAME row, PRESERVED config (not wiped to `{}`).
    let c = store
        .connections()
        .ensure(ws.id, ConnectionKind::Email, "mbox", None, None)
        .await
        .expect("ensure None");
    assert_eq!(a.id, c.id);
    let cfg_c = store.connections().get_row(ws.id, c.id).await.unwrap();
    assert_eq!(
        cfg_c.config()["root"],
        "/mail/b",
        "config is preserved on re-ensure with None"
    );
}

/// Config reconciliation preserves row identity while replacing both config and
/// credential exactly (including clearing a previous credential).
#[tokio::test]
async fn configured_connection_reconcile_is_exact_and_keeps_identity() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping configured_connection_reconcile_is_exact_and_keeps_identity: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };

    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create(
            "configured",
            &format!("configured-{}", uuid::Uuid::new_v4()),
        )
        .await
        .expect("ws");
    let first = store
        .connections()
        .reconcile_configured(
            ws.id,
            ConnectionKind::Postgres,
            "reporting",
            Some("old-ref"),
            serde_json::json!({"host": "old"}),
        )
        .await
        .expect("first reconcile");
    let second = store
        .connections()
        .reconcile_configured(
            ws.id,
            ConnectionKind::Postgres,
            "reporting",
            None,
            serde_json::json!({"host": "new"}),
        )
        .await
        .expect("second reconcile");
    assert_eq!(first.id, second.id);
    assert_eq!(second.credential_ref, None);
    let row = store
        .connections()
        .get_row(ws.id, second.id)
        .await
        .expect("row");
    assert_eq!(row.config.0, serde_json::json!({"host": "new"}));
}

/// `set_watch_state` (SOUL §8/§16 M7 push half): the Google push-channel state
/// rides the `config.watch` key without clobbering the user-set provider keys, and
/// clears cleanly (the key is removed, not left as `null`). Skips offline.
#[tokio::test]
async fn connection_set_watch_state_rides_config_and_clears() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping connection_set_watch_state_rides_config_and_clears: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };

    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("watch", &format!("watch-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let conn = store
        .connections()
        .create(
            ws.id,
            ConnectionKind::Calendar,
            "gcal",
            Some("cred-ref"),
            Some(serde_json::json!({ "provider": "google", "calendar": "primary" })),
        )
        .await
        .expect("create google cal");

    // Set the watch state → it lands under `config.watch`, alongside the untouched
    // provider keys.
    let watch = serde_json::json!({
        "channel_id": "chan-1",
        "resource_id": "res-9",
        "expiry": "2026-07-09T00:00:00Z"
    });
    store
        .connections()
        .set_watch_state(ws.id, conn.id, Some(watch.clone()))
        .await
        .expect("set watch state");
    let row = store.connections().get_row(ws.id, conn.id).await.unwrap();
    assert_eq!(row.config()["watch"], watch, "watch state persisted");
    assert_eq!(
        row.config()["provider"],
        "google",
        "provider key untouched (additive write)"
    );
    assert_eq!(row.config()["calendar"], "primary");

    // Clear it → the key is removed entirely (not stored as `null`), provider keys
    // still intact.
    store
        .connections()
        .set_watch_state(ws.id, conn.id, None)
        .await
        .expect("clear watch state");
    let row = store.connections().get_row(ws.id, conn.id).await.unwrap();
    assert!(
        row.config().get("watch").is_none(),
        "watch key removed on clear, got {:?}",
        row.config().get("watch")
    );
    assert_eq!(
        row.config()["provider"],
        "google",
        "provider survives a clear"
    );
}

/// `set_credential_ref` + `scrub_config_keys` — the minimal set-credential + config
/// mutations opportunistic Gmail resealing relies on (SOUL §13/§28).
/// `set_credential_ref` swaps a connection's sealed-credential pointer (and clears
/// it) touching only that column, and `scrub_config_keys` deletes named plaintext
/// keys **additively** (leaving the rest of `config` intact, like `set_watch_state`),
/// so the two never race a concurrent config writer. Skips offline.
#[tokio::test]
async fn connection_set_credential_ref_and_scrub_config_keys_round_trip() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping connection_set_credential_ref_and_scrub_config_keys_round_trip: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };

    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("cred", &format!("cred-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");

    // A legacy plaintext Gmail connection: no credential_ref, the plaintext OAuth
    // triplet + the provider/label keys in config.
    let conn = store
        .connections()
        .create(
            ws.id,
            ConnectionKind::Email,
            "Gmail",
            None,
            Some(serde_json::json!({
                "provider": "gmail",
                "label": "INBOX",
                "client_id": "cid",
                "client_secret": "sec",
                "refresh_token": "rt"
            })),
        )
        .await
        .expect("create plaintext gmail");
    assert!(
        conn.credential_ref.is_none(),
        "starts with no credential_ref"
    );

    // set_credential_ref points it at a sealed secret — config is left untouched.
    let updated = store
        .connections()
        .set_credential_ref(ws.id, conn.id, Some("sec-abc"))
        .await
        .expect("set credential_ref");
    assert_eq!(updated.credential_ref.as_deref(), Some("sec-abc"));
    let row = store.connections().get_row(ws.id, conn.id).await.unwrap();
    assert_eq!(
        row.config()["client_id"],
        "cid",
        "set_credential_ref does not touch config"
    );

    // scrub_config_keys strips exactly the plaintext triplet, leaving provider/label
    // and the credential_ref intact (additive delete, not a whole-blob overwrite).
    let scrubbed = store
        .connections()
        .scrub_config_keys(
            ws.id,
            conn.id,
            &["client_id", "client_secret", "refresh_token"],
        )
        .await
        .expect("scrub plaintext keys");
    assert_eq!(
        scrubbed.credential_ref.as_deref(),
        Some("sec-abc"),
        "scrub leaves credential_ref intact"
    );
    let row = store.connections().get_row(ws.id, conn.id).await.unwrap();
    assert!(
        row.config().get("client_id").is_none(),
        "client_id scrubbed"
    );
    assert!(
        row.config().get("client_secret").is_none(),
        "client_secret scrubbed"
    );
    assert!(
        row.config().get("refresh_token").is_none(),
        "refresh_token scrubbed"
    );
    assert_eq!(
        row.config()["provider"],
        "gmail",
        "provider survives the scrub"
    );
    assert_eq!(row.config()["label"], "INBOX", "label survives the scrub");

    // Re-scrubbing an already-absent key is a no-op (idempotent re-run).
    store
        .connections()
        .scrub_config_keys(ws.id, conn.id, &["client_id"])
        .await
        .expect("scrub is idempotent");
    let row = store.connections().get_row(ws.id, conn.id).await.unwrap();
    assert_eq!(row.config()["provider"], "gmail");

    // set_credential_ref(None) clears the pointer (detach / rotation restart).
    let cleared = store
        .connections()
        .set_credential_ref(ws.id, conn.id, None)
        .await
        .expect("clear credential_ref");
    assert!(
        cleared.credential_ref.is_none(),
        "credential_ref cleared to None"
    );
}

#[tokio::test]
async fn connection_update_named_config_replaces_and_is_isolated() {
    // The edit-source path (SOUL §28): `update_named_config` is a full replace of
    // a connection's name + config, leaving credential_ref/sync_token untouched,
    // workspace-scoped like every other repo call (§18).
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping connection_update_named_config_replaces_and_is_isolated: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("connupd", &format!("connupd-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let other = store
        .workspaces()
        .create("connupd-b", &format!("connupd-b-{}", uuid::Uuid::new_v4()))
        .await
        .expect("other ws");

    let conn = store
        .connections()
        .create(
            ws.id,
            ConnectionKind::Email,
            "Work inbox",
            Some("vault:cred-1"),
            Some(serde_json::json!({
                "provider": "imap", "host": "old.example.com", "username": "me",
                "password": "old-secret", "mailbox": "INBOX"
            })),
        )
        .await
        .expect("create");

    let updated = store
        .connections()
        .update_named_config(
            ws.id,
            conn.id,
            "Work inbox (fastmail)",
            serde_json::json!({
                "provider": "imap", "host": "imap.fastmail.com", "username": "me",
                "password": "old-secret", "mailbox": "Archive"
            }),
        )
        .await
        .expect("update");
    assert_eq!(updated.id, conn.id, "update keeps the id");
    assert_eq!(updated.name, "Work inbox (fastmail)");
    assert_eq!(
        updated.credential_ref.as_deref(),
        Some("vault:cred-1"),
        "credential_ref untouched by a name/config update"
    );
    let row = store.connections().get_row(ws.id, conn.id).await.unwrap();
    assert_eq!(row.config()["host"], "imap.fastmail.com");
    assert_eq!(row.config()["mailbox"], "Archive", "full config replace");

    // §18: a foreign workspace can neither see nor update it.
    assert!(matches!(
        store
            .connections()
            .update_named_config(other.id, conn.id, "hijack", serde_json::json!({}))
            .await,
        Err(StoreError::NotFound)
    ));
    let row = store.connections().get_row(ws.id, conn.id).await.unwrap();
    assert_eq!(
        row.config()["host"],
        "imap.fastmail.com",
        "cross-workspace update attempt left the row untouched"
    );
}
