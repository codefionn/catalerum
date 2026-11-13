use catalerum_core::id::{OrganisationId, UserId, WorkspaceId};
use catalerum_core::model::{CreationPolicy, Membership, Organisation, OrgMembership, OrgRole, Role};
#[cfg(not(feature = "sqlite"))]
use uuid::Uuid;
use crate::error::{Result, StoreError};
use crate::rows::{
    creation_policy_to_text, MembershipRow, org_role_to_text, OrganisationRow, OrgMembershipRow,
    role_to_text,
};
use super::{map, PgPool};

// ===========================================================================
// Memberships
// ===========================================================================

/// CRUD for the `memberships` table (workspace ⇄ user, with a role).
#[derive(Clone, Debug)]
pub struct MembershipRepo {
    pool: PgPool,
}

impl MembershipRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create (or upsert) a membership, returning the stored binding.
    pub async fn upsert(
        &self,
        workspace_id: WorkspaceId,
        user_id: UserId,
        role: Role,
    ) -> Result<Membership> {
        let role_text = role_to_text(role)?;
        let row: MembershipRow = sqlx::query_as(
            "INSERT INTO memberships (workspace_id, user_id, role)
             VALUES ($1, $2, $3)
             ON CONFLICT (workspace_id, user_id)
             DO UPDATE SET role = EXCLUDED.role
             RETURNING workspace_id, user_id, role, created_at",
        )
        .bind(workspace_id.into_uuid())
        .bind(user_id.into_uuid())
        .bind(role_text)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Fetch a single membership, or [`StoreError::NotFound`].
    pub async fn get(&self, workspace_id: WorkspaceId, user_id: UserId) -> Result<Membership> {
        let row: MembershipRow = sqlx::query_as(
            "SELECT workspace_id, user_id, role, created_at
             FROM memberships WHERE workspace_id = $1 AND user_id = $2",
        )
        .bind(workspace_id.into_uuid())
        .bind(user_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// List all memberships in a workspace.
    pub async fn list_by_workspace(&self, workspace_id: WorkspaceId) -> Result<Vec<Membership>> {
        let rows: Vec<MembershipRow> = sqlx::query_as(
            "SELECT workspace_id, user_id, role, created_at
             FROM memberships WHERE workspace_id = $1
             ORDER BY created_at ASC",
        )
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(Membership::try_from).collect()
    }

    /// List all workspace memberships a user holds.
    pub async fn list_by_user(&self, user_id: UserId) -> Result<Vec<Membership>> {
        let rows: Vec<MembershipRow> = sqlx::query_as(
            "SELECT workspace_id, user_id, role, created_at
             FROM memberships WHERE user_id = $1
             ORDER BY created_at ASC",
        )
        .bind(user_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(Membership::try_from).collect()
    }

    /// Remove a user from a workspace.
    pub async fn delete(&self, workspace_id: WorkspaceId, user_id: UserId) -> Result<()> {
        let res = sqlx::query("DELETE FROM memberships WHERE workspace_id = $1 AND user_id = $2")
            .bind(workspace_id.into_uuid())
            .bind(user_id.into_uuid())
            .execute(&self.pool)
            .await
            .map_err(map)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }
}

// ===========================================================================
// Organisations
// ===========================================================================

/// CRUD for the `organisations` table — the administrative grouping above
/// workspaces (SOUL §18). Org roles govern administration only; they confer no
/// data access.
#[derive(Clone, Debug)]
pub struct OrganisationRepo {
    pool: PgPool,
}

impl OrganisationRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Insert an organisation with a fresh id and the given workspace-creation
    /// policy, returning the stored row.
    ///
    /// # Errors
    /// [`StoreError::Conflict`] if the slug is already taken.
    pub async fn create(
        &self,
        name: &str,
        slug: &str,
        workspace_creation: CreationPolicy,
    ) -> Result<Organisation> {
        let id = OrganisationId::new().into_uuid();
        let policy = creation_policy_to_text(workspace_creation)?;
        let row: OrganisationRow = sqlx::query_as(
            "INSERT INTO organisations (id, name, slug, workspace_creation)
             VALUES ($1, $2, $3, $4)
             RETURNING id, name, slug, workspace_creation, created_at, updated_at",
        )
        .bind(id)
        .bind(name)
        .bind(slug)
        .bind(policy)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Fetch an organisation by id, or [`StoreError::NotFound`].
    pub async fn get(&self, id: OrganisationId) -> Result<Organisation> {
        let row: OrganisationRow = sqlx::query_as(
            "SELECT id, name, slug, workspace_creation, created_at, updated_at
             FROM organisations WHERE id = $1",
        )
        .bind(id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Fetch an organisation by its unique slug.
    pub async fn get_by_slug(&self, slug: &str) -> Result<Organisation> {
        let row: OrganisationRow = sqlx::query_as(
            "SELECT id, name, slug, workspace_creation, created_at, updated_at
             FROM organisations WHERE slug = $1",
        )
        .bind(slug)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// List all organisations, newest first.
    pub async fn list(&self) -> Result<Vec<Organisation>> {
        let rows: Vec<OrganisationRow> = sqlx::query_as(
            "SELECT id, name, slug, workspace_creation, created_at, updated_at
             FROM organisations ORDER BY created_at DESC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(Organisation::try_from).collect()
    }

    /// Fetch multiple organisations by id in **one** query (order unspecified;
    /// absent ids omitted) — resolves a user's org memberships → org details
    /// without an N+1.
    #[cfg(not(feature = "sqlite"))]
    pub async fn get_many(&self, ids: &[OrganisationId]) -> Result<Vec<Organisation>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let uuids: Vec<Uuid> = ids.iter().map(OrganisationId::as_uuid).collect();
        let rows: Vec<OrganisationRow> = sqlx::query_as(
            "SELECT id, name, slug, workspace_creation, created_at, updated_at
             FROM organisations WHERE id = ANY($1)",
        )
        .bind(&uuids)
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(Organisation::try_from).collect()
    }

    #[cfg(feature = "sqlite")]
    pub async fn get_many(&self, ids: &[OrganisationId]) -> Result<Vec<Organisation>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
            "SELECT id, name, slug, workspace_creation, created_at, updated_at \
             FROM organisations WHERE id IN (",
        );
        let mut values = query.separated(", ");
        for id in ids {
            values.push_bind(id.as_uuid());
        }
        values.push_unseparated(")");
        let rows: Vec<OrganisationRow> = query
            .build_query_as()
            .fetch_all(&self.pool)
            .await
            .map_err(map)?;
        rows.into_iter().map(Organisation::try_from).collect()
    }

    /// Update an organisation's workspace-creation policy (org admin/owner only,
    /// enforced at the API layer). Returns the updated row.
    pub async fn set_workspace_creation(
        &self,
        id: OrganisationId,
        workspace_creation: CreationPolicy,
    ) -> Result<Organisation> {
        let policy = creation_policy_to_text(workspace_creation)?;
        let row: OrganisationRow = sqlx::query_as(
            "UPDATE organisations SET workspace_creation = $2, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1
             RETURNING id, name, slug, workspace_creation, created_at, updated_at",
        )
        .bind(id.into_uuid())
        .bind(policy)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Delete an organisation by id, returning whether a row was removed.
    ///
    /// The `org_memberships.organisation_id` FK is `ON DELETE CASCADE`
    /// (migration `0046`), so the org's memberships are removed with it — no
    /// explicit membership cleanup is needed. The `workspaces.organisation_id` FK
    /// has **no** cascade, so a delete while any workspace (live *or* archived) is
    /// still attached errors out — a fail-closed backstop under the API's own
    /// "no workspaces" precondition (SOUL §18). Deletion is reserved for empty,
    /// non-default organisations; the API layer enforces owner-only + those
    /// preconditions before calling this.
    pub async fn delete(&self, id: OrganisationId) -> Result<bool> {
        let res = sqlx::query("DELETE FROM organisations WHERE id = $1")
            .bind(id.into_uuid())
            .execute(&self.pool)
            .await
            .map_err(map)?;
        Ok(res.rows_affected() > 0)
    }
}

// ===========================================================================
// Organisation memberships
// ===========================================================================

/// CRUD for the `org_memberships` table (organisation ⇄ user, with an
/// administrative role, SOUL §18).
#[derive(Clone, Debug)]
pub struct OrgMembershipRepo {
    pool: PgPool,
}

impl OrgMembershipRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create (or upsert) an org membership, returning the stored binding.
    pub async fn upsert(
        &self,
        organisation_id: OrganisationId,
        user_id: UserId,
        role: OrgRole,
    ) -> Result<OrgMembership> {
        let role_text = org_role_to_text(role)?;
        let row: OrgMembershipRow = sqlx::query_as(
            "INSERT INTO org_memberships (organisation_id, user_id, role)
             VALUES ($1, $2, $3)
             ON CONFLICT (organisation_id, user_id)
             DO UPDATE SET role = EXCLUDED.role
             RETURNING organisation_id, user_id, role, created_at",
        )
        .bind(organisation_id.into_uuid())
        .bind(user_id.into_uuid())
        .bind(role_text)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Fetch a single org membership, or [`StoreError::NotFound`].
    pub async fn get(
        &self,
        organisation_id: OrganisationId,
        user_id: UserId,
    ) -> Result<OrgMembership> {
        let row: OrgMembershipRow = sqlx::query_as(
            "SELECT organisation_id, user_id, role, created_at
             FROM org_memberships WHERE organisation_id = $1 AND user_id = $2",
        )
        .bind(organisation_id.into_uuid())
        .bind(user_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// List all memberships in an organisation.
    pub async fn list_by_organisation(
        &self,
        organisation_id: OrganisationId,
    ) -> Result<Vec<OrgMembership>> {
        let rows: Vec<OrgMembershipRow> = sqlx::query_as(
            "SELECT organisation_id, user_id, role, created_at
             FROM org_memberships WHERE organisation_id = $1
             ORDER BY created_at ASC",
        )
        .bind(organisation_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(OrgMembership::try_from).collect()
    }

    /// List all org memberships a user holds.
    pub async fn list_by_user(&self, user_id: UserId) -> Result<Vec<OrgMembership>> {
        let rows: Vec<OrgMembershipRow> = sqlx::query_as(
            "SELECT organisation_id, user_id, role, created_at
             FROM org_memberships WHERE user_id = $1
             ORDER BY created_at ASC",
        )
        .bind(user_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(OrgMembership::try_from).collect()
    }

    /// Remove a user from an organisation. Returns whether a row was removed.
    pub async fn delete(&self, organisation_id: OrganisationId, user_id: UserId) -> Result<bool> {
        let res =
            sqlx::query("DELETE FROM org_memberships WHERE organisation_id = $1 AND user_id = $2")
                .bind(organisation_id.into_uuid())
                .bind(user_id.into_uuid())
                .execute(&self.pool)
                .await
                .map_err(map)?;
        Ok(res.rows_affected() > 0)
    }
}
