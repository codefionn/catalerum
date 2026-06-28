//! Organisations REST surface (SOUL §18) — the administrative grouping above
//! workspaces.
//!
//! An **organisation** groups workspaces for administration only: creating /
//! archiving workspaces in the org, managing org members, and org-level policy.
//! Org roles (Owner / Admin / Member) confer **no** data access — reading a
//! workspace still requires a workspace membership, and organisations never
//! appear in capability strings (§19). Creation is policy-gated, deny-by-default:
//!
//! - `GET  /organisations` — the caller's organisations, each with the workspaces
//!   in it the caller can actually see (their workspace memberships).
//! - `POST /organisations` — create an organisation (instance policy
//!   `organisation_creation`); the creator becomes the org **Owner**.
//! - `DELETE /organisations/{id}` — delete an organisation (org **Owner** only —
//!   stricter than admin, deletion is structural). Fail-closed: the seeded
//!   **default** organisation is never deletable (`409`), and an org that holds
//!   **any** workspace — live *or* archived — is undeletable (`409`), since
//!   workspaces can only be archived (never deleted), so only an org that never
//!   held a workspace can be removed. The org row + its memberships go (the
//!   `org_memberships` FK cascades, migration `0046`).
//! - `GET  /organisations/{id}/members` — list org members (org admin/owner).
//! - `POST /organisations/{id}/members` — add/update an org member (org admin/owner;
//!   only an Owner may grant/modify Owner).
//! - `DELETE /organisations/{id}/members/{user_id}` — remove an org member (org
//!   admin/owner; the last Owner cannot be removed).
//! - `PUT  /organisations/{id}/policy` — set the org's `workspace_creation` policy
//!   (org admin/owner).
//! - `GET  /organisations/{id}/workspaces` — list every workspace **shell** in the
//!   org (org admin/owner; ids/names only, no data — the shell an org admin
//!   administers even without a workspace membership). **Archived** shells are
//!   included and flagged (`archived_at` set) so an admin can restore them.
//! - `POST /organisations/{id}/workspaces` — create a workspace in the org (org
//!   policy `workspace_creation`); the creator becomes the workspace **Owner**.
//! - `DELETE /organisations/{id}/workspaces/{ws_id}` — **soft-archive** a
//!   workspace shell (org admin/owner). A reversible archive (`archived_at`
//!   stamped): the workspace + its data are retained but hidden from every default
//!   listing and can no longer be switched into. Hard delete is **no longer
//!   exposed** on the API — archive is the only removal path.
//! - `POST /organisations/{id}/workspaces/{ws_id}/restore` — **restore** an
//!   archived workspace shell (org admin/owner; clears `archived_at`).
//!
//! Everything is enforced through the `Auth` extractor + explicit org-role checks,
//! independent of the workspace capability gate — an org admin who is not a member
//! of a workspace administers its shell, never its contents (SOUL §18).

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use catalerum_core::model::{CreationPolicy, OrgRole, Role};
use catalerum_core::{OrganisationId, UserId, WorkspaceId};
use catalerum_store::{Store, StoreError};

use crate::auth::Auth;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Mount the organisation routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/organisations", get(list_my_orgs).post(create_org))
        .route("/organisations/{id}", delete(delete_org))
        .route(
            "/organisations/{id}/members",
            get(list_members).post(add_member),
        )
        .route("/organisations/{id}/user-lookup", get(user_lookup))
        .route(
            "/organisations/{id}/members/{user_id}",
            delete(remove_member),
        )
        .route("/organisations/{id}/policy", put(set_policy))
        .route(
            "/organisations/{id}/workspaces",
            get(list_org_workspaces).post(create_workspace),
        )
        .route(
            "/organisations/{id}/workspaces/{ws_id}",
            delete(archive_workspace),
        )
        .route(
            "/organisations/{id}/workspaces/{ws_id}/restore",
            post(restore_workspace),
        )
}

// ---------------------------------------------------------------------------
// Policy <-> string helpers (lowercase, matching the core snake_case serde form)
// ---------------------------------------------------------------------------

fn policy_str(policy: CreationPolicy) -> &'static str {
    match policy {
        CreationPolicy::Disabled => "disabled",
        CreationPolicy::Admins => "admins",
        CreationPolicy::Members => "members",
    }
}

fn policy_from_str(s: &str) -> ApiResult<CreationPolicy> {
    match s.trim() {
        "disabled" => Ok(CreationPolicy::Disabled),
        "admins" => Ok(CreationPolicy::Admins),
        "members" => Ok(CreationPolicy::Members),
        other => Err(ApiError::bad_request(format!(
            "unknown creation policy `{other}` (expected disabled|admins|members)"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Shared org-role resolution helpers (deny-by-default, SOUL §18)
// ---------------------------------------------------------------------------

/// The caller's role in an organisation, or `None` if they are not a member.
/// Takes the bare [`Store`] (not `AppState`) so the org-role gate is DB-testable
/// without standing up the whole app state.
async fn caller_org_role(
    store: &Store,
    org_id: OrganisationId,
    user_id: UserId,
) -> ApiResult<Option<OrgRole>> {
    match store.org_memberships().get(org_id, user_id).await {
        Ok(m) => Ok(Some(m.role)),
        Err(StoreError::NotFound) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Require the caller to be an org admin (owner or admin) of `org_id`, returning
/// their role. A non-member — or a plain Member — is `403` (the org shell is
/// administered by owners/admins only, SOUL §18).
///
/// `is_scoped` is `auth.grant().is_some()`: a **grant-scoped** token can NEVER
/// administer an organisation, whatever its minter's org role. Org administration
/// is a role-derived privilege a capability-bounded token does not inherit (the
/// grant carries workspace *capabilities*, never org authority) — exactly like
/// [`Auth::require_workspace_admin`](crate::auth::Auth::require_workspace_admin).
/// The scoped token fails closed **before** the DB lookup (SOUL §19: a scoped
/// token is strictly *less* than its minter).
async fn require_org_admin(
    store: &Store,
    org_id: OrganisationId,
    user_id: UserId,
    is_scoped: bool,
) -> ApiResult<OrgRole> {
    if is_scoped {
        return Err(ApiError::Forbidden(
            "grant-scoped tokens cannot administer organisations".into(),
        ));
    }
    let role = caller_org_role(store, org_id, user_id)
        .await?
        .ok_or_else(|| ApiError::Forbidden("you are not a member of this organisation".into()))?;
    if !catalerum_iam::is_org_admin(role) {
        return Err(ApiError::Forbidden(
            "organisation admin (owner/admin) required".into(),
        ));
    }
    Ok(role)
}

/// Require the caller to be an org **Owner** of `org_id`. Stricter than
/// [`require_org_admin`]: a plain Admin — like a Member or non-member — is `403`.
/// Structural operations (deleting the whole organisation) are Owner-only, since
/// they cannot be undone (SOUL §18).
///
/// `is_scoped` fails a grant-scoped token closed before the DB lookup, exactly as
/// in [`require_org_admin`] (org administration is not a capability a scoped token
/// can carry, SOUL §19).
async fn require_org_owner(
    store: &Store,
    org_id: OrganisationId,
    user_id: UserId,
    is_scoped: bool,
) -> ApiResult<OrgRole> {
    if is_scoped {
        return Err(ApiError::Forbidden(
            "grant-scoped tokens cannot administer organisations".into(),
        ));
    }
    let role = caller_org_role(store, org_id, user_id)
        .await?
        .ok_or_else(|| ApiError::Forbidden("you are not a member of this organisation".into()))?;
    if !catalerum_iam::is_org_owner(role) {
        return Err(ApiError::Forbidden("organisation Owner required".into()));
    }
    Ok(role)
}

// ---------------------------------------------------------------------------
// GET /organisations — my organisations + the workspaces I can see in each
// ---------------------------------------------------------------------------

/// A workspace the caller is a member of, within an organisation.
#[derive(Debug, Serialize)]
pub struct MyWorkspace {
    pub id: WorkspaceId,
    pub name: String,
    pub slug: String,
    /// The caller's workspace role token (`owner`/`admin`/`member`/`viewer`).
    pub role: String,
}

/// One of the caller's organisations, with the workspaces in it they can see.
#[derive(Debug, Serialize)]
pub struct MyOrganisation {
    pub id: OrganisationId,
    pub name: String,
    pub slug: String,
    /// The caller's org role token (`owner`/`admin`/`member`).
    pub role: String,
    /// The org's workspace-creation policy (`disabled`/`admins`/`members`).
    pub workspace_creation: String,
    /// Whether the caller may create a workspace here under the org policy — the
    /// server's own deny-by-default verdict, so the UI need not re-derive it.
    pub can_create_workspace: bool,
    /// The workspaces in this org the caller is a member of (their data boundary),
    /// **not** every workspace in the org (org roles confer no data access).
    pub workspaces: Vec<MyWorkspace>,
}

async fn list_my_orgs(
    State(state): State<AppState>,
    auth: Auth,
) -> ApiResult<Json<Vec<MyOrganisation>>> {
    let p = auth.principal();

    // The caller's org memberships → org details (one query each).
    let org_memberships = state
        .store()
        .org_memberships()
        .list_by_user(p.user_id)
        .await?;
    let org_ids: Vec<_> = org_memberships.iter().map(|m| m.organisation_id).collect();
    let orgs_by_id: std::collections::HashMap<_, _> = state
        .store()
        .organisations()
        .get_many(&org_ids)
        .await?
        .into_iter()
        .map(|o| (o.id, o))
        .collect();

    // The caller's workspace memberships → workspace details, so we can bucket the
    // workspaces the caller can see under their organisation.
    let ws_memberships = state.store().memberships().list_by_user(p.user_id).await?;
    let ws_role: std::collections::HashMap<_, _> = ws_memberships
        .iter()
        .map(|m| (m.workspace_id, m.role))
        .collect();
    let ws_ids: Vec<_> = ws_memberships.iter().map(|m| m.workspace_id).collect();
    let workspaces = state.store().workspaces().get_many(&ws_ids).await?;

    let mut out = Vec::with_capacity(org_memberships.len());
    for m in &org_memberships {
        let Some(org) = orgs_by_id.get(&m.organisation_id) else {
            continue;
        };
        let mut mine: Vec<MyWorkspace> = workspaces
            .iter()
            // Hide archived workspaces from the caller's own org→workspace view
            // (`get_many` returns archived rows; this is a user-facing listing).
            .filter(|ws| ws.organisation_id == org.id && ws.archived_at.is_none())
            .map(|ws| MyWorkspace {
                id: ws.id,
                name: ws.name.clone(),
                slug: ws.slug.clone(),
                role: ws_role
                    .get(&ws.id)
                    .map(|r| catalerum_iam::role_str(*r).to_string())
                    .unwrap_or_default(),
            })
            .collect();
        mine.sort_by(|a, b| a.name.cmp(&b.name));
        out.push(MyOrganisation {
            id: org.id,
            name: org.name.clone(),
            slug: org.slug.clone(),
            role: catalerum_iam::org_role_str(m.role).to_string(),
            workspace_creation: policy_str(org.workspace_creation).to_string(),
            can_create_workspace: catalerum_iam::workspace_creation_allowed(
                org.workspace_creation,
                Some(m.role),
            ),
            workspaces: mine,
        });
    }
    Ok(Json(out))
}

// ---------------------------------------------------------------------------
// POST /organisations — create an organisation (instance policy gated)
// ---------------------------------------------------------------------------

/// Body for `POST /organisations`.
#[derive(Debug, Deserialize)]
pub struct CreateOrg {
    pub name: String,
    pub slug: String,
}

async fn create_org(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<CreateOrg>,
) -> ApiResult<(StatusCode, Json<catalerum_core::model::Organisation>)> {
    let p = auth.principal();
    let name = body.name.trim();
    let slug = body.slug.trim().to_lowercase();
    if name.is_empty() || slug.is_empty() {
        return Err(ApiError::bad_request("name and slug are required"));
    }

    // Instance policy `organisation_creation`, deny-by-default (SOUL §18). Under
    // `admins`, only a user who already owns/admins some org may create a new one.
    let policy = state.config().server.effective_organisation_creation();
    let is_admin_somewhere = state
        .store()
        .org_memberships()
        .list_by_user(p.user_id)
        .await?
        .iter()
        .any(|m| catalerum_iam::is_org_admin(m.role));
    if !catalerum_iam::organisation_creation_allowed(policy, is_admin_somewhere) {
        return Err(ApiError::Forbidden(format!(
            "organisation creation is not permitted (instance policy: {})",
            policy_str(policy)
        )));
    }

    // New orgs inherit the deployment mode's default workspace-creation policy
    // (members in single-user, admins in multi-user) — applied at creation time.
    let default_ws_policy = state.config().server.default_workspace_creation();
    let org = state
        .store()
        .organisations()
        .create(name, &slug, default_ws_policy)
        .await?;
    // The creator becomes the organisation's Owner (SOUL §18).
    state
        .store()
        .org_memberships()
        .upsert(org.id, p.user_id, OrgRole::Owner)
        .await?;
    Ok((StatusCode::CREATED, Json(org)))
}

// ---------------------------------------------------------------------------
// DELETE /organisations/{id}  (org Owner only; empty, non-default orgs only)
// ---------------------------------------------------------------------------

/// The structural preconditions for deleting an organisation (pure — no DB), given
/// whether it is the seeded **default** org and how many workspace **shells** it
/// holds (live + archived, from the include-archived listing). Deny-by-default:
///
/// - the default org anchors the backfill + the org-less `create` default
///   (migration `0046`) and is never deletable;
/// - an org with **any** workspace is undeletable — workspaces can only be
///   *archived* (never deleted), so an org that ever held one can never be
///   emptied; deletion is reserved for orgs that never held a workspace.
///
/// Returns the `409 Conflict` to raise, or `Ok(())` to proceed. The owner-only
/// gate is applied separately (it needs the DB).
fn org_delete_precondition(is_default: bool, workspace_shells: usize) -> ApiResult<()> {
    if is_default {
        return Err(ApiError::Conflict(
            "the default organisation cannot be deleted".into(),
        ));
    }
    if workspace_shells > 0 {
        return Err(ApiError::Conflict(
            "this organisation still has workspaces (archived workspaces count too — \
             workspaces can only be archived, never deleted, so an organisation with \
             any workspace is not deletable)"
                .into(),
        ));
    }
    Ok(())
}

async fn delete_org(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<OrganisationId>,
) -> ApiResult<StatusCode> {
    let p = auth.principal();
    // Structural: Owner-only (stricter than the admin gate the rest of the surface
    // uses — deletion cannot be undone, SOUL §18). A grant-scoped token can never
    // reach here (§19).
    require_org_owner(state.store(), id, p.user_id, auth.grant().is_some()).await?;

    // Resolve the org (a 404 if it does not exist) so we can guard the default org.
    let org = state.store().organisations().get(id).await?;
    let is_default = id == catalerum_iam::DEFAULT_ORGANISATION_ID
        || org.slug == catalerum_iam::DEFAULT_ORGANISATION_SLUG;

    // Include-archived listing: any workspace at all (live or archived) blocks it.
    let shells = state
        .store()
        .workspaces()
        .list_by_organisation_including_archived(id)
        .await?;
    org_delete_precondition(is_default, shells.len())?;

    // Remove the org row; its `org_memberships` cascade away (FK ON DELETE CASCADE,
    // migration `0046`) — no explicit membership cleanup needed.
    if state.store().organisations().delete(id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound)
    }
}

// ---------------------------------------------------------------------------
// GET /organisations/{id}/members  (org admin/owner)
// ---------------------------------------------------------------------------

/// A member of an organisation, for the members panel.
#[derive(Debug, Serialize)]
pub struct OrgMemberView {
    pub user_id: UserId,
    pub email: String,
    pub display_name: String,
    /// Org role token (`owner`/`admin`/`member`).
    pub role: String,
}

async fn list_members(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<OrganisationId>,
) -> ApiResult<Json<Vec<OrgMemberView>>> {
    let p = auth.principal();
    require_org_admin(state.store(), id, p.user_id, auth.grant().is_some()).await?;
    let members = state
        .store()
        .org_memberships()
        .list_by_organisation(id)
        .await?;
    let mut out = Vec::with_capacity(members.len());
    for m in members {
        // Best-effort user detail; skip a membership whose user has vanished.
        if let Ok(user) = state.store().users().get(m.user_id).await {
            out.push(OrgMemberView {
                user_id: m.user_id,
                email: user.email,
                display_name: user.display_name,
                role: catalerum_iam::org_role_str(m.role).to_string(),
            });
        }
    }
    Ok(Json(out))
}

// ---------------------------------------------------------------------------
// GET /organisations/{id}/user-lookup?email=…  (org admin/owner)
// ---------------------------------------------------------------------------
//
// A **minimal** email→user resolver for the add-member flow: an org admin already
// adds members by user id, so letting them resolve one member's id by that member's
// exact email adds no new authority. It is intentionally *not* a search endpoint —
// exact (case-insensitive) address only, no substrings/prefixes, and a **no-match
// is an opaque 404** (indistinguishable from an unknown org) so it can't be used to
// enumerate accounts. Gated exactly like add-member: org admin/owner, deny-by-default.

/// Query for `GET /organisations/{id}/user-lookup`.
#[derive(Debug, Deserialize)]
pub struct UserLookupQuery {
    /// The exact email address to resolve (case-insensitive; no wildcards).
    pub email: String,
}

/// The resolved user for the add-member flow — just enough to add them by id and
/// confirm who was matched. No membership/role data (that is the members listing).
#[derive(Debug, Serialize)]
pub struct UserLookupView {
    pub user_id: UserId,
    pub email: String,
    /// Omitted when the user has no display name set.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub display_name: String,
}

async fn user_lookup(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<OrganisationId>,
    Query(q): Query<UserLookupQuery>,
) -> ApiResult<Json<UserLookupView>> {
    let p = auth.principal();
    // Gate first (org admin/owner) so a non-admin can never probe for accounts —
    // they get a 403 before any lookup runs (SOUL §18, deny-by-default).
    require_org_admin(state.store(), id, p.user_id, auth.grant().is_some()).await?;

    let email = q.email.trim();
    if email.is_empty() {
        return Err(ApiError::bad_request("email is required"));
    }
    // Exact, case-insensitive match. A miss is an opaque 404 (no "user exists"
    // signal) — the endpoint resolves a known address, it does not enumerate.
    match state.store().users().get_by_email_ci(email).await {
        Ok(user) => Ok(Json(UserLookupView {
            user_id: user.id,
            email: user.email,
            display_name: user.display_name,
        })),
        Err(StoreError::NotFound) => Err(ApiError::NotFound),
        Err(e) => Err(e.into()),
    }
}

// ---------------------------------------------------------------------------
// POST /organisations/{id}/members  (org admin/owner; only Owner grants Owner)
// ---------------------------------------------------------------------------

/// Body for `POST /organisations/{id}/members`.
#[derive(Debug, Deserialize)]
pub struct AddMember {
    pub user_id: UserId,
    /// Org role token (`owner`/`admin`/`member`).
    pub role: String,
}

async fn add_member(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<OrganisationId>,
    Json(body): Json<AddMember>,
) -> ApiResult<Json<OrgMemberView>> {
    let p = auth.principal();
    let caller_role =
        require_org_admin(state.store(), id, p.user_id, auth.grant().is_some()).await?;
    let new_role = catalerum_iam::org_role_from_str(body.role.trim())?;

    // Only an Owner may grant the Owner role or modify an existing Owner — an Admin
    // administers members/admins but cannot mint peers above themselves (§18).
    let target_current = caller_org_role(state.store(), id, body.user_id).await?;
    let touches_owner = new_role == OrgRole::Owner || target_current == Some(OrgRole::Owner);
    if touches_owner && !catalerum_iam::is_org_owner(caller_role) {
        return Err(ApiError::Forbidden(
            "only an organisation Owner may grant or modify the Owner role".into(),
        ));
    }

    // The target must be a known user (a real principal, not an arbitrary id).
    let user = state
        .store()
        .users()
        .get(body.user_id)
        .await
        .map_err(|_| ApiError::bad_request("unknown user"))?;

    // Guard the last Owner: demoting the sole Owner would orphan the org's
    // administration.
    if target_current == Some(OrgRole::Owner) && new_role != OrgRole::Owner {
        ensure_not_last_owner(state.store(), id, body.user_id).await?;
    }

    state
        .store()
        .org_memberships()
        .upsert(id, body.user_id, new_role)
        .await?;
    Ok(Json(OrgMemberView {
        user_id: body.user_id,
        email: user.email,
        display_name: user.display_name,
        role: catalerum_iam::org_role_str(new_role).to_string(),
    }))
}

// ---------------------------------------------------------------------------
// DELETE /organisations/{id}/members/{user_id}  (org admin/owner)
// ---------------------------------------------------------------------------

async fn remove_member(
    State(state): State<AppState>,
    auth: Auth,
    Path((id, user_id)): Path<(OrganisationId, UserId)>,
) -> ApiResult<StatusCode> {
    let p = auth.principal();
    let caller_role =
        require_org_admin(state.store(), id, p.user_id, auth.grant().is_some()).await?;

    // Only an Owner may remove an Owner (§18).
    let target_current = caller_org_role(state.store(), id, user_id).await?;
    if target_current == Some(OrgRole::Owner) {
        if !catalerum_iam::is_org_owner(caller_role) {
            return Err(ApiError::Forbidden(
                "only an organisation Owner may remove an Owner".into(),
            ));
        }
        ensure_not_last_owner(state.store(), id, user_id).await?;
    }

    let removed = state.store().org_memberships().delete(id, user_id).await?;
    if removed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound)
    }
}

/// Refuse an operation that would remove/demote the **last** Owner of an org — the
/// org must always retain at least one Owner to stay administrable (SOUL §18).
async fn ensure_not_last_owner(
    store: &Store,
    org_id: OrganisationId,
    target: UserId,
) -> ApiResult<()> {
    let owners = store
        .org_memberships()
        .list_by_organisation(org_id)
        .await?
        .into_iter()
        .filter(|m| m.role == OrgRole::Owner)
        .count();
    // If the target is the only Owner, block it.
    let target_is_owner = matches!(
        caller_org_role(store, org_id, target).await?,
        Some(OrgRole::Owner)
    );
    if owners <= 1 && target_is_owner {
        return Err(ApiError::bad_request(
            "cannot remove or demote the last organisation Owner",
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// PUT /organisations/{id}/policy  (org admin/owner)
// ---------------------------------------------------------------------------

/// Body for `PUT /organisations/{id}/policy`.
#[derive(Debug, Deserialize)]
pub struct SetPolicy {
    /// New `workspace_creation` policy (`disabled`/`admins`/`members`).
    pub workspace_creation: String,
}

async fn set_policy(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<OrganisationId>,
    Json(body): Json<SetPolicy>,
) -> ApiResult<Json<catalerum_core::model::Organisation>> {
    let p = auth.principal();
    require_org_admin(state.store(), id, p.user_id, auth.grant().is_some()).await?;
    let policy = policy_from_str(&body.workspace_creation)?;
    let org = state
        .store()
        .organisations()
        .set_workspace_creation(id, policy)
        .await?;
    Ok(Json(org))
}

// ---------------------------------------------------------------------------
// GET /organisations/{id}/workspaces  (org admin/owner — all shells in the org)
// ---------------------------------------------------------------------------

/// A workspace shell (id/name/slug only — no data) an org admin administers.
#[derive(Debug, Serialize)]
pub struct WorkspaceShell {
    pub id: WorkspaceId,
    pub name: String,
    pub slug: String,
    /// When the shell was **soft-archived** (SOUL §18), or absent while active.
    /// Present so the org-admin panel can flag archived workspaces and offer a
    /// restore action — this listing includes archived shells for that reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<chrono::DateTime<chrono::Utc>>,
}

async fn list_org_workspaces(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<OrganisationId>,
) -> ApiResult<Json<Vec<WorkspaceShell>>> {
    let p = auth.principal();
    require_org_admin(state.store(), id, p.user_id, auth.grant().is_some()).await?;
    // Include archived shells here (unlike every user-facing listing) so an admin
    // sees what can be restored; each is flagged by its `archived_at` (SOUL §18).
    let shells = state
        .store()
        .workspaces()
        .list_by_organisation_including_archived(id)
        .await?
        .into_iter()
        .map(|ws| WorkspaceShell {
            id: ws.id,
            name: ws.name,
            slug: ws.slug,
            archived_at: ws.archived_at,
        })
        .collect();
    Ok(Json(shells))
}

// ---------------------------------------------------------------------------
// POST /organisations/{id}/workspaces  (org policy gated; creator → Owner)
// ---------------------------------------------------------------------------

/// Body for `POST /organisations/{id}/workspaces`.
#[derive(Debug, Deserialize)]
pub struct CreateWorkspace {
    pub name: String,
    pub slug: String,
}

async fn create_workspace(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<OrganisationId>,
    Json(body): Json<CreateWorkspace>,
) -> ApiResult<(StatusCode, Json<catalerum_core::model::Workspace>)> {
    let p = auth.principal();
    let name = body.name.trim();
    let slug = body.slug.trim().to_lowercase();
    if name.is_empty() || slug.is_empty() {
        return Err(ApiError::bad_request("name and slug are required"));
    }

    // Resolve the org + the caller's org role, then apply the org's
    // `workspace_creation` policy, deny-by-default (SOUL §18).
    let org = state.store().organisations().get(id).await?;
    let caller_role = caller_org_role(state.store(), id, p.user_id).await?;
    if !catalerum_iam::workspace_creation_allowed(org.workspace_creation, caller_role) {
        return Err(ApiError::Forbidden(format!(
            "workspace creation is not permitted in this organisation (policy: {})",
            policy_str(org.workspace_creation)
        )));
    }

    let ws = state
        .store()
        .workspaces()
        .create_in_org(id, name, &slug)
        .await?;
    // The creator becomes the workspace Owner — a new workspace starts empty
    // (nothing shared across workspaces at creation, SOUL §18).
    state
        .store()
        .memberships()
        .upsert(ws.id, p.user_id, Role::Owner)
        .await?;
    Ok((StatusCode::CREATED, Json(ws)))
}

// ---------------------------------------------------------------------------
// DELETE /organisations/{id}/workspaces/{ws_id}  (org admin/owner — soft-archive)
// ---------------------------------------------------------------------------

async fn archive_workspace(
    State(state): State<AppState>,
    auth: Auth,
    Path((id, ws_id)): Path<(OrganisationId, WorkspaceId)>,
) -> ApiResult<StatusCode> {
    let p = auth.principal();
    require_org_admin(state.store(), id, p.user_id, auth.grant().is_some()).await?;

    // The workspace must belong to this org (an org admin administers only their
    // own org's shells, SOUL §18). `get` returns the row whether or not archived.
    let ws = state.store().workspaces().get(ws_id).await?;
    if ws.organisation_id != id {
        return Err(ApiError::NotFound);
    }
    // Soft-archive: stamp `archived_at` (reversible via restore). The workspace +
    // its data are retained but hidden from every default listing and can no longer
    // be switched into (SOUL §18). Hard delete is no longer exposed on the API.
    state.store().workspaces().archive(ws_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// POST /organisations/{id}/workspaces/{ws_id}/restore  (org admin/owner)
// ---------------------------------------------------------------------------

async fn restore_workspace(
    State(state): State<AppState>,
    auth: Auth,
    Path((id, ws_id)): Path<(OrganisationId, WorkspaceId)>,
) -> ApiResult<Json<WorkspaceShell>> {
    let p = auth.principal();
    require_org_admin(state.store(), id, p.user_id, auth.grant().is_some()).await?;

    // Same org-ownership gate as archive — resolve the (possibly archived) shell
    // first and confirm it belongs to this org (SOUL §18).
    let ws = state.store().workspaces().get(ws_id).await?;
    if ws.organisation_id != id {
        return Err(ApiError::NotFound);
    }
    // Clear `archived_at`: the workspace reappears in listings and can be switched
    // into again. Idempotent on an already-active workspace.
    let restored = state.store().workspaces().unarchive(ws_id).await?;
    Ok(Json(WorkspaceShell {
        id: restored.id,
        name: restored.name,
        slug: restored.slug,
        archived_at: restored.archived_at,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_string_round_trip() {
        for p in [
            CreationPolicy::Disabled,
            CreationPolicy::Admins,
            CreationPolicy::Members,
        ] {
            assert_eq!(policy_from_str(policy_str(p)).unwrap(), p);
        }
        assert!(policy_from_str("bogus").is_err());
    }

    #[test]
    fn create_org_body_decodes() {
        let b: CreateOrg = serde_json::from_str(r#"{"name":"Acme","slug":"acme"}"#).unwrap();
        assert_eq!(b.name, "Acme");
        assert_eq!(b.slug, "acme");
    }

    #[test]
    fn add_member_body_decodes() {
        let b: AddMember = serde_json::from_str(
            r#"{"user_id":"11111111-1111-1111-1111-111111111111","role":"admin"}"#,
        )
        .unwrap();
        assert_eq!(b.role, "admin");
    }

    /// The lookup response carries the resolved id + email, and omits the display
    /// name only when it is blank (so the web add-by-email flow can show who it
    /// matched without a spurious empty field).
    #[test]
    fn user_lookup_view_omits_blank_display_name() {
        let named = UserLookupView {
            user_id: UserId::new(),
            email: "a@b.test".into(),
            display_name: "Ada".into(),
        };
        let j = serde_json::to_value(&named).unwrap();
        assert_eq!(j["email"], serde_json::json!("a@b.test"));
        assert_eq!(j["display_name"], serde_json::json!("Ada"));

        let anon = UserLookupView {
            user_id: UserId::new(),
            email: "x@y.test".into(),
            display_name: String::new(),
        };
        let j = serde_json::to_value(&anon).unwrap();
        assert!(
            j.get("display_name").is_none(),
            "a blank display name is omitted"
        );
    }

    fn db_url() -> Option<String> {
        std::env::var("CATALERUM_TEST_DATABASE_URL")
            .or_else(|_| std::env::var("DATABASE_URL"))
            .ok()
    }

    /// The user-lookup gate + opaque 404 (SOUL §18): the org-admin gate lets an
    /// owner/admin through and fails a plain member / non-member closed (403), and
    /// the resolver matches an email case-insensitively while an unknown address is
    /// an opaque `NotFound` — never an enumeration signal.
    #[tokio::test]
    async fn user_lookup_gate_matches_ci_and_404s_opaquely() {
        let Some(url) = db_url() else {
            eprintln!(
                "skipping user_lookup_gate test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
            );
            return;
        };
        let store = crate::test_db::isolated_store(&url).await;

        // An org with an Owner, a plain Member, and a target user to resolve.
        let org = store
            .organisations()
            .create(
                "Acme",
                &format!("acme-{}", uuid::Uuid::new_v4()),
                CreationPolicy::Members,
            )
            .await
            .expect("org");
        let owner = store
            .users()
            .create(
                &format!("owner-{}@ex.test", uuid::Uuid::new_v4()),
                "Owner",
                None,
            )
            .await
            .expect("owner");
        let member = store
            .users()
            .create(
                &format!("member-{}@ex.test", uuid::Uuid::new_v4()),
                "Member",
                None,
            )
            .await
            .expect("member");
        // The lookup target has a mixed-case address so the CI match is meaningful.
        let target_email = format!("Target-{}@Ex.Test", uuid::Uuid::new_v4());
        let target = store
            .users()
            .create(&target_email, "Target", None)
            .await
            .expect("target");
        store
            .org_memberships()
            .upsert(org.id, owner.id, OrgRole::Owner)
            .await
            .expect("owner membership");
        store
            .org_memberships()
            .upsert(org.id, member.id, OrgRole::Member)
            .await
            .expect("member membership");

        // Gate: owner/admin pass; a plain member and a non-member are 403.
        // (`false` = a role-derived, non-scoped token; the scoped case is covered by
        // `org_admin_owner_gates_reject_a_grant_scoped_token`.)
        assert!(require_org_admin(&store, org.id, owner.id, false)
            .await
            .is_ok());
        assert!(matches!(
            require_org_admin(&store, org.id, member.id, false).await,
            Err(ApiError::Forbidden(_))
        ));
        assert!(matches!(
            require_org_admin(&store, org.id, UserId::new(), false).await,
            Err(ApiError::Forbidden(_))
        ));

        // Case-insensitive exact match resolves the same user regardless of casing.
        let hit = store
            .users()
            .get_by_email_ci(&target_email.to_lowercase())
            .await
            .expect("lower-case match resolves");
        assert_eq!(hit.id, target.id);
        let hit_upper = store
            .users()
            .get_by_email_ci(&target_email.to_uppercase())
            .await
            .expect("upper-case match resolves");
        assert_eq!(hit_upper.id, target.id);

        // An unknown address is an opaque miss (the route maps this to 404).
        let miss = store
            .users()
            .get_by_email_ci(&format!("nobody-{}@ex.test", uuid::Uuid::new_v4()))
            .await;
        assert!(matches!(miss, Err(StoreError::NotFound)));
    }

    /// The org-admin workspaces listing flags archived shells (`archived_at`
    /// present) and omits the field for active ones — the contract the web
    /// org-admin panel relies on to offer a restore action (SOUL §18).
    #[test]
    fn workspace_shell_flags_archived_only_when_set() {
        let active = WorkspaceShell {
            id: WorkspaceId::new(),
            name: "Live".into(),
            slug: "live".into(),
            archived_at: None,
        };
        let j = serde_json::to_value(&active).unwrap();
        assert!(
            j.get("archived_at").is_none(),
            "active shell omits archived_at"
        );

        let archived = WorkspaceShell {
            id: WorkspaceId::new(),
            name: "Retired".into(),
            slug: "retired".into(),
            archived_at: Some(chrono::Utc::now()),
        };
        let j = serde_json::to_value(&archived).unwrap();
        assert!(
            j.get("archived_at").is_some(),
            "archived shell carries archived_at so the admin can restore it"
        );
    }

    /// The delete-organisation preconditions (pure): the default org is never
    /// deletable, any workspace (live or archived, hence any positive shell count)
    /// blocks deletion, and only a non-default org with no workspaces at all is
    /// deletable (SOUL §18).
    #[test]
    fn org_delete_precondition_denies_default_and_non_empty() {
        // Default org: never deletable, even when it holds no workspaces.
        assert!(matches!(
            org_delete_precondition(true, 0),
            Err(ApiError::Conflict(_))
        ));
        // Any workspace shell (live or archived) blocks deletion with a 409.
        assert!(matches!(
            org_delete_precondition(false, 1),
            Err(ApiError::Conflict(_))
        ));
        assert!(matches!(
            org_delete_precondition(false, 5),
            Err(ApiError::Conflict(_))
        ));
        // A non-default org with no workspaces at all is deletable.
        assert!(org_delete_precondition(false, 0).is_ok());
    }

    /// The delete gate + precondition against real store data (SOUL §18): the
    /// owner-only gate lets an Owner through and fails an Admin / Member /
    /// non-member closed (403); an empty org deletes (cascading its memberships);
    /// and an org with a workspace — counted from the include-archived listing,
    /// even after it is archived — is 409 (non-deletable).
    #[tokio::test]
    async fn delete_org_gate_is_owner_only_and_precondition_uses_include_archived() {
        let Some(url) = db_url() else {
            eprintln!(
                "skipping delete_org gate test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
            );
            return;
        };
        let store = crate::test_db::isolated_store(&url).await;

        let org = store
            .organisations()
            .create(
                "Del Co",
                &format!("del-{}", uuid::Uuid::new_v4()),
                CreationPolicy::Members,
            )
            .await
            .expect("org");
        let owner = store
            .users()
            .create(
                &format!("o-{}@ex.test", uuid::Uuid::new_v4()),
                "Owner",
                None,
            )
            .await
            .expect("owner");
        let admin = store
            .users()
            .create(
                &format!("a-{}@ex.test", uuid::Uuid::new_v4()),
                "Admin",
                None,
            )
            .await
            .expect("admin");
        let member = store
            .users()
            .create(
                &format!("m-{}@ex.test", uuid::Uuid::new_v4()),
                "Member",
                None,
            )
            .await
            .expect("member");
        store
            .org_memberships()
            .upsert(org.id, owner.id, OrgRole::Owner)
            .await
            .expect("owner m");
        store
            .org_memberships()
            .upsert(org.id, admin.id, OrgRole::Admin)
            .await
            .expect("admin m");
        store
            .org_memberships()
            .upsert(org.id, member.id, OrgRole::Member)
            .await
            .expect("member m");

        // Owner-only gate: the Owner passes; an Admin, a Member, and a non-member
        // are all 403 (stricter than the admin gate the rest of the surface uses).
        assert!(require_org_owner(&store, org.id, owner.id, false)
            .await
            .is_ok());
        assert!(matches!(
            require_org_owner(&store, org.id, admin.id, false).await,
            Err(ApiError::Forbidden(_))
        ));
        assert!(matches!(
            require_org_owner(&store, org.id, member.id, false).await,
            Err(ApiError::Forbidden(_))
        ));
        assert!(matches!(
            require_org_owner(&store, org.id, UserId::new(), false).await,
            Err(ApiError::Forbidden(_))
        ));

        // Empty org ⇒ precondition passes; the repo delete removes it and cascades
        // all three memberships (org_memberships FK ON DELETE CASCADE).
        let shells = store
            .workspaces()
            .list_by_organisation_including_archived(org.id)
            .await
            .expect("shells");
        assert!(org_delete_precondition(false, shells.len()).is_ok());
        assert!(store.organisations().delete(org.id).await.expect("delete"));
        assert!(matches!(
            store.organisations().get(org.id).await,
            Err(StoreError::NotFound)
        ));
        assert!(store
            .org_memberships()
            .list_by_organisation(org.id)
            .await
            .expect("memberships gone")
            .is_empty());

        // An org that holds a workspace is non-deletable — and it stays non-deletable
        // once archived, because the count comes from the include-archived listing.
        let org2 = store
            .organisations()
            .create(
                "Full Co",
                &format!("full-{}", uuid::Uuid::new_v4()),
                CreationPolicy::Members,
            )
            .await
            .expect("org2");
        let ws = store
            .workspaces()
            .create_in_org(org2.id, "W", &format!("w-{}", uuid::Uuid::new_v4()))
            .await
            .expect("ws");
        store.workspaces().archive(ws.id).await.expect("archive");
        let with_archived = store
            .workspaces()
            .list_by_organisation_including_archived(org2.id)
            .await
            .expect("shells2");
        assert_eq!(with_archived.len(), 1, "archived workspaces still count");
        assert!(matches!(
            org_delete_precondition(false, with_archived.len()),
            Err(ApiError::Conflict(_))
        ));
    }

    /// A **grant-scoped** token can never administer an organisation, whatever its
    /// minter's org role (SOUL §19 — a scoped token is strictly *less* than its
    /// minter, mirroring `Auth::require_workspace_admin`). The org Owner passes both
    /// gates with a role-derived token (`is_scoped = false`) but is rejected the
    /// instant the token is grant-scoped (`auth.grant().is_some()`), fail-closed
    /// **before** the DB lookup — so a narrow token cannot act as its minting owner.
    #[tokio::test]
    async fn org_admin_owner_gates_reject_a_grant_scoped_token() {
        let Some(url) = db_url() else {
            eprintln!(
                "skipping scoped-org-gate test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
            );
            return;
        };
        let store = crate::test_db::isolated_store(&url).await;

        let org = store
            .organisations()
            .create(
                "Scoped Co",
                &format!("scoped-{}", uuid::Uuid::new_v4()),
                CreationPolicy::Members,
            )
            .await
            .expect("org");
        let owner = store
            .users()
            .create(
                &format!("o-{}@ex.test", uuid::Uuid::new_v4()),
                "Owner",
                None,
            )
            .await
            .expect("owner");
        store
            .org_memberships()
            .upsert(org.id, owner.id, OrgRole::Owner)
            .await
            .expect("owner membership");

        // Baseline: a role-derived (non-scoped) token for the Owner passes both gates.
        assert!(require_org_admin(&store, org.id, owner.id, false)
            .await
            .is_ok());
        assert!(require_org_owner(&store, org.id, owner.id, false)
            .await
            .is_ok());

        // A grant-scoped token minted by that same Owner carries only workspace
        // capabilities, never org authority. `auth.grant().is_some()` is exactly the
        // signal the routes thread in, and both gates fail it closed (403).
        use catalerum_core::capability::{Action, Capability, Resource};
        use catalerum_core::model::Grant;
        use catalerum_core::GrantId;
        let ws = WorkspaceId::new();
        let grant = Grant {
            id: GrantId::new(),
            workspace_id: ws,
            name: "notes-only".into(),
            capabilities: vec![Capability::new(Action::Read, Resource::domain("notes"))],
            constraints: Default::default(),
        };
        let mut p = catalerum_iam::Principal::new(owner.id, ws, Role::Owner);
        p.grant_id = Some(grant.id);
        let auth = Auth::with_grant(p, grant);
        assert!(auth.grant().is_some(), "the token is grant-scoped");

        assert!(matches!(
            require_org_admin(&store, org.id, owner.id, auth.grant().is_some()).await,
            Err(ApiError::Forbidden(_))
        ));
        assert!(matches!(
            require_org_owner(&store, org.id, owner.id, auth.grant().is_some()).await,
            Err(ApiError::Forbidden(_))
        ));
    }
}
