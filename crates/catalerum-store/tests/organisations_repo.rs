//! Integration test: `OrganisationRepo` + `OrgMembershipRepo` CRUD, the default
//! organisation seed + workspace backfill/default attachment, and
//! `WorkspaceRepo::create_in_org` / `list_by_organisation` (SOUL §18).
//!
//! DB-gated like the other store tests: set `CATALERUM_TEST_DATABASE_URL` (or
//! `DATABASE_URL`) to run it; otherwise it skips and passes offline.

use catalerum_core::model::{CreationPolicy, OrgRole, Role};
use catalerum_store::{Store, DEFAULT_ORGANISATION_SLUG};

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

#[tokio::test]
async fn default_org_seeded_and_backfills_new_workspaces() {
    let Some(url) = test_db_url() else {
        eprintln!("skipping default_org_seeded_and_backfills_new_workspaces: set CATALERUM_TEST_DATABASE_URL");
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");

    // The `0046` migration seeds the well-known default organisation.
    let default_org = store
        .organisations()
        .get_by_slug(DEFAULT_ORGANISATION_SLUG)
        .await
        .expect("default org seeded");
    assert_eq!(default_org.slug, "default");
    // Its id matches the fixed literal shared with `catalerum_iam::DEFAULT_ORGANISATION_ID`.
    assert_eq!(
        default_org.id.to_string(),
        "def00000-0000-4000-8000-000000000000"
    );

    // The org-less `create(name, slug)` convenience attaches the new workspace to
    // the default org (the backfill/default path the dev seed + store tests use).
    let ws = store
        .workspaces()
        .create("orgless", &format!("orgless-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    assert_eq!(
        ws.organisation_id, default_org.id,
        "org-less create defaults to the default organisation"
    );

    // And it is listed under the default org.
    let in_org = store
        .workspaces()
        .list_by_organisation(default_org.id)
        .await
        .expect("list by org");
    assert!(in_org.iter().any(|w| w.id == ws.id));
}

#[tokio::test]
async fn org_crud_and_workspace_creation() {
    let Some(url) = test_db_url() else {
        eprintln!("skipping org_crud_and_workspace_creation: set CATALERUM_TEST_DATABASE_URL");
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");

    let slug = format!("acme-{}", uuid::Uuid::new_v4());
    let org = store
        .organisations()
        .create("Acme", &slug, CreationPolicy::Admins)
        .await
        .expect("create org");
    assert_eq!(org.workspace_creation, CreationPolicy::Admins);

    // Round-trips by id + slug.
    assert_eq!(store.organisations().get(org.id).await.unwrap().id, org.id);
    assert_eq!(
        store.organisations().get_by_slug(&slug).await.unwrap().id,
        org.id
    );

    // Policy update persists.
    let updated = store
        .organisations()
        .set_workspace_creation(org.id, CreationPolicy::Members)
        .await
        .expect("set policy");
    assert_eq!(updated.workspace_creation, CreationPolicy::Members);

    // A workspace created in the org carries its organisation_id.
    let ws = store
        .workspaces()
        .create_in_org(org.id, "Team", &format!("team-{}", uuid::Uuid::new_v4()))
        .await
        .expect("create_in_org");
    assert_eq!(ws.organisation_id, org.id);

    // get_many resolves the org among others.
    let many = store.organisations().get_many(&[org.id]).await.unwrap();
    assert_eq!(many.len(), 1);
    assert_eq!(many[0].id, org.id);
}

#[tokio::test]
async fn workspace_soft_archive_hides_from_listings_but_get_still_resolves() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping workspace_soft_archive_hides_from_listings_but_get_still_resolves: \
             set CATALERUM_TEST_DATABASE_URL"
        );
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");

    let org = store
        .organisations()
        .create(
            "Archive Co",
            &format!("archive-{}", uuid::Uuid::new_v4()),
            CreationPolicy::Members,
        )
        .await
        .expect("org");
    let ws = store
        .workspaces()
        .create_in_org(
            org.id,
            "Doomed",
            &format!("doomed-{}", uuid::Uuid::new_v4()),
        )
        .await
        .expect("create_in_org");
    // Active on creation.
    assert!(ws.archived_at.is_none());

    // Visible in the default per-org listing + the global listing while active.
    assert!(store
        .workspaces()
        .list_by_organisation(org.id)
        .await
        .unwrap()
        .iter()
        .any(|w| w.id == ws.id));
    assert!(store
        .workspaces()
        .list()
        .await
        .unwrap()
        .iter()
        .any(|w| w.id == ws.id));

    // Archive: stamps `archived_at`, returns the updated row.
    let archived = store.workspaces().archive(ws.id).await.expect("archive");
    assert!(archived.archived_at.is_some());

    // Hidden from BOTH default listings now.
    assert!(!store
        .workspaces()
        .list_by_organisation(org.id)
        .await
        .unwrap()
        .iter()
        .any(|w| w.id == ws.id));
    assert!(!store
        .workspaces()
        .list()
        .await
        .unwrap()
        .iter()
        .any(|w| w.id == ws.id));

    // But `get` (identity lookup) still resolves it, carrying the archive stamp —
    // restore + org-admin views depend on this.
    let fetched = store.workspaces().get(ws.id).await.expect("get archived");
    assert!(fetched.archived_at.is_some());

    // And the include-archived org listing surfaces it (flagged) so an admin can
    // restore it.
    let all = store
        .workspaces()
        .list_by_organisation_including_archived(org.id)
        .await
        .unwrap();
    assert!(all.iter().any(|w| w.id == ws.id && w.archived_at.is_some()));

    // Unarchive: clears the stamp and the workspace reappears in the default
    // listing.
    let restored = store
        .workspaces()
        .unarchive(ws.id)
        .await
        .expect("unarchive");
    assert!(restored.archived_at.is_none());
    assert!(store
        .workspaces()
        .list_by_organisation(org.id)
        .await
        .unwrap()
        .iter()
        .any(|w| w.id == ws.id));
}

#[tokio::test]
async fn org_delete_removes_org_and_cascades_memberships() {
    let Some(url) = test_db_url() else {
        eprintln!("skipping org_delete_removes_org_and_cascades_memberships: set CATALERUM_TEST_DATABASE_URL");
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");

    // An empty org with an Owner membership — the deletable shape (SOUL §18).
    let org = store
        .organisations()
        .create(
            "Doomed Org",
            &format!("doomed-org-{}", uuid::Uuid::new_v4()),
            CreationPolicy::Members,
        )
        .await
        .expect("org");
    let user = store
        .users()
        .create(
            &format!("owner-{}@x.test", uuid::Uuid::new_v4()),
            "Owner",
            None,
        )
        .await
        .expect("user");
    store
        .org_memberships()
        .upsert(org.id, user.id, OrgRole::Owner)
        .await
        .expect("owner membership");

    // Delete removes the org row and returns true.
    assert!(store.organisations().delete(org.id).await.expect("delete"));
    assert!(matches!(
        store.organisations().get(org.id).await,
        Err(catalerum_store::StoreError::NotFound)
    ));

    // The membership cascaded away with the org (FK ON DELETE CASCADE) — the user
    // no longer administers it.
    assert!(store.org_memberships().get(org.id, user.id).await.is_err());
    assert!(!store
        .org_memberships()
        .list_by_user(user.id)
        .await
        .unwrap()
        .iter()
        .any(|m| m.organisation_id == org.id));

    // Idempotent: a second delete removes nothing.
    assert!(!store
        .organisations()
        .delete(org.id)
        .await
        .expect("re-delete"));
}

#[tokio::test]
async fn org_memberships_crud_and_isolation() {
    let Some(url) = test_db_url() else {
        eprintln!("skipping org_memberships_crud_and_isolation: set CATALERUM_TEST_DATABASE_URL");
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");

    let org = store
        .organisations()
        .create(
            "Org",
            &format!("org-{}", uuid::Uuid::new_v4()),
            CreationPolicy::Members,
        )
        .await
        .expect("org");
    let other = store
        .organisations()
        .create(
            "Other",
            &format!("other-{}", uuid::Uuid::new_v4()),
            CreationPolicy::Members,
        )
        .await
        .expect("other org");

    // A user to bind (org membership FKs into users).
    let user = store
        .users()
        .create(&format!("u-{}@x.test", uuid::Uuid::new_v4()), "U", None)
        .await
        .expect("user");

    // Upsert = Member, then promote to Owner (idempotent by key).
    let m = store
        .org_memberships()
        .upsert(org.id, user.id, OrgRole::Member)
        .await
        .expect("upsert member");
    assert_eq!(m.role, OrgRole::Member);
    let m2 = store
        .org_memberships()
        .upsert(org.id, user.id, OrgRole::Owner)
        .await
        .expect("promote");
    assert_eq!(m2.role, OrgRole::Owner);

    // get + list_by_organisation + list_by_user.
    assert_eq!(
        store
            .org_memberships()
            .get(org.id, user.id)
            .await
            .unwrap()
            .role,
        OrgRole::Owner
    );
    assert_eq!(
        store
            .org_memberships()
            .list_by_organisation(org.id)
            .await
            .unwrap()
            .len(),
        1
    );
    let by_user = store.org_memberships().list_by_user(user.id).await.unwrap();
    assert!(by_user.iter().any(|m| m.organisation_id == org.id));

    // Isolation: the other org has no membership for this user.
    assert!(store
        .org_memberships()
        .get(other.id, user.id)
        .await
        .is_err());
    assert!(store
        .org_memberships()
        .list_by_organisation(other.id)
        .await
        .unwrap()
        .is_empty());

    // Delete is idempotent (true then false).
    assert!(store
        .org_memberships()
        .delete(org.id, user.id)
        .await
        .unwrap());
    assert!(!store
        .org_memberships()
        .delete(org.id, user.id)
        .await
        .unwrap());

    // Org roles confer no *data* access: a workspace still needs a workspace
    // membership. Sanity-check that the two membership tables are independent —
    // an org membership does not create a workspace membership.
    let ws = store
        .workspaces()
        .create_in_org(org.id, "W", &format!("w-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    store
        .org_memberships()
        .upsert(org.id, user.id, OrgRole::Admin)
        .await
        .expect("re-add org admin");
    // The user administers the org shell but is NOT a workspace member.
    assert!(store.memberships().get(ws.id, user.id).await.is_err());
    // Adding a workspace membership is a separate, explicit act.
    store
        .memberships()
        .upsert(ws.id, user.id, Role::Owner)
        .await
        .expect("ws membership");
    assert_eq!(
        store.memberships().get(ws.id, user.id).await.unwrap().role,
        Role::Owner
    );
}
