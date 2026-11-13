use catalerum_core::id::{OrganisationId, UserId, WorkspaceId};
use catalerum_core::model::{User, Workspace};
use sqlx::types::Json;
use uuid::Uuid;
use crate::error::{Result, StoreError};
use crate::rows::{UserRow, WorkspaceRow};
use super::{DEFAULT_ORGANISATION_SLUG, map, PgPool};

// ===========================================================================
// Workspaces
// ===========================================================================

/// CRUD for the `workspaces` table.
#[derive(Clone, Debug)]
pub struct WorkspaceRepo {
    pool: PgPool,
}

impl WorkspaceRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Insert a workspace in the **default** organisation and return the stored
    /// row (SOUL §18). The default org is resolved by its well-known slug
    /// ([`DEFAULT_ORGANISATION_SLUG`]); the `0046` migration seeds it. Callers that
    /// know the target organisation should use
    /// [`create_in_org`](Self::create_in_org) instead — this convenience keeps the
    /// pre-organisation `create(name, slug)` surface working (the dev seed + the
    /// store tests) by defaulting the org.
    pub async fn create(&self, name: &str, slug: &str) -> Result<Workspace> {
        let id = WorkspaceId::new().into_uuid();
        let row: WorkspaceRow = sqlx::query_as(
            "INSERT INTO workspaces (id, organisation_id, name, slug)
             VALUES ($1, (SELECT id FROM organisations WHERE slug = $2), $3, $4)
             RETURNING id, organisation_id, name, slug, created_at, updated_at, archived_at",
        )
        .bind(id)
        .bind(DEFAULT_ORGANISATION_SLUG)
        .bind(name)
        .bind(slug)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Insert a workspace in a specific organisation and return the stored row
    /// (SOUL §18) — the policy-gated `create workspace within an org` path. The
    /// `organisation_id` FK is enforced by the DB, so an unknown org is rejected.
    pub async fn create_in_org(
        &self,
        organisation_id: OrganisationId,
        name: &str,
        slug: &str,
    ) -> Result<Workspace> {
        let id = WorkspaceId::new().into_uuid();
        let row: WorkspaceRow = sqlx::query_as(
            "INSERT INTO workspaces (id, organisation_id, name, slug)
             VALUES ($1, $2, $3, $4)
             RETURNING id, organisation_id, name, slug, created_at, updated_at, archived_at",
        )
        .bind(id)
        .bind(organisation_id.into_uuid())
        .bind(name)
        .bind(slug)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch a workspace by id, or [`StoreError::NotFound`]. Returns the row
    /// **whether or not it is archived** — restore + org-admin views need to
    /// resolve archived workspaces (SOUL §18); the default *listings* hide them.
    pub async fn get(&self, id: WorkspaceId) -> Result<Workspace> {
        let row: WorkspaceRow = sqlx::query_as(
            "SELECT id, organisation_id, name, slug, created_at, updated_at, archived_at
             FROM workspaces WHERE id = $1",
        )
        .bind(id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch a workspace by its unique slug. Like [`get`](Self::get) this is an
    /// identity lookup and returns the row even when archived.
    pub async fn get_by_slug(&self, slug: &str) -> Result<Workspace> {
        let row: WorkspaceRow = sqlx::query_as(
            "SELECT id, organisation_id, name, slug, created_at, updated_at, archived_at
             FROM workspaces WHERE slug = $1",
        )
        .bind(slug)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// List all **active** (non-archived) workspaces, newest first. Archived
    /// workspaces are excluded by default (SOUL §18) — background surfaces that
    /// resolve workspaces through this listing (storage watch, ingest schedule)
    /// therefore skip archived workspaces naturally.
    pub async fn list(&self) -> Result<Vec<Workspace>> {
        let rows: Vec<WorkspaceRow> = sqlx::query_as(
            "SELECT id, organisation_id, name, slug, created_at, updated_at, archived_at
             FROM workspaces WHERE archived_at IS NULL ORDER BY created_at DESC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Workspace::from).collect())
    }

    /// List the **active** (non-archived) workspaces belonging to one organisation,
    /// newest first (SOUL §18). Archived workspaces are excluded — use
    /// [`list_by_organisation_including_archived`](Self::list_by_organisation_including_archived)
    /// for the org-admin shell view that must surface archived workspaces to restore.
    pub async fn list_by_organisation(
        &self,
        organisation_id: OrganisationId,
    ) -> Result<Vec<Workspace>> {
        let rows: Vec<WorkspaceRow> = sqlx::query_as(
            "SELECT id, organisation_id, name, slug, created_at, updated_at, archived_at
             FROM workspaces WHERE organisation_id = $1 AND archived_at IS NULL
             ORDER BY created_at DESC",
        )
        .bind(organisation_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Workspace::from).collect())
    }

    /// List **every** workspace in an organisation — active and archived —
    /// newest first (SOUL §18). The org-admin `GET /organisations/{id}/workspaces`
    /// listing uses this so an admin sees archived shells (flagged by
    /// `archived_at`) and can restore them. Not for user-facing listings.
    pub async fn list_by_organisation_including_archived(
        &self,
        organisation_id: OrganisationId,
    ) -> Result<Vec<Workspace>> {
        let rows: Vec<WorkspaceRow> = sqlx::query_as(
            "SELECT id, organisation_id, name, slug, created_at, updated_at, archived_at
             FROM workspaces WHERE organisation_id = $1
             ORDER BY created_at DESC",
        )
        .bind(organisation_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Workspace::from).collect())
    }

    /// Fetch multiple workspaces by id in **one** query (order unspecified; absent
    /// ids are simply omitted). Like [`get`](Self::get) this is an identity lookup:
    /// archived workspaces are returned (each carries its `archived_at`), so a
    /// caller building a user-facing listing must drop archived rows itself.
    #[cfg(not(feature = "sqlite"))]
    pub async fn get_many(&self, ids: &[WorkspaceId]) -> Result<Vec<Workspace>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let uuids: Vec<Uuid> = ids.iter().map(WorkspaceId::as_uuid).collect();
        let rows: Vec<WorkspaceRow> = sqlx::query_as(
            "SELECT id, organisation_id, name, slug, created_at, updated_at, archived_at
             FROM workspaces WHERE id = ANY($1)",
        )
        .bind(&uuids)
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Workspace::from).collect())
    }

    #[cfg(feature = "sqlite")]
    pub async fn get_many(&self, ids: &[WorkspaceId]) -> Result<Vec<Workspace>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
            "SELECT id, organisation_id, name, slug, created_at, updated_at, archived_at \
             FROM workspaces WHERE id IN (",
        );
        let mut values = query.separated(", ");
        for id in ids {
            values.push_bind(id.as_uuid());
        }
        values.push_unseparated(")");
        let rows: Vec<WorkspaceRow> = query
            .build_query_as()
            .fetch_all(&self.pool)
            .await
            .map_err(map)?;
        Ok(rows.into_iter().map(Workspace::from).collect())
    }

    /// Rename a workspace and/or change its slug. Returns the updated row.
    pub async fn update(&self, id: WorkspaceId, name: &str, slug: &str) -> Result<Workspace> {
        let row: WorkspaceRow = sqlx::query_as(
            "UPDATE workspaces SET name = $2, slug = $3, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1
             RETURNING id, organisation_id, name, slug, created_at, updated_at, archived_at",
        )
        .bind(id.into_uuid())
        .bind(name)
        .bind(slug)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// **Soft-archive** a workspace: stamp `archived_at = CURRENT_TIMESTAMP` (SOUL §18). The
    /// row and all its data are retained; the workspace vanishes from every default
    /// listing and can no longer be switched into, but an org admin can
    /// [`unarchive`](Self::unarchive) it. This replaced the former hard delete for
    /// the org-admin archive action. Re-archiving an already-archived workspace just
    /// refreshes the timestamp. Returns the updated row, or [`StoreError::NotFound`].
    pub async fn archive(&self, id: WorkspaceId) -> Result<Workspace> {
        let row: WorkspaceRow = sqlx::query_as(
            "UPDATE workspaces SET archived_at = CURRENT_TIMESTAMP, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1
             RETURNING id, organisation_id, name, slug, created_at, updated_at, archived_at",
        )
        .bind(id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// **Restore** an archived workspace: clear `archived_at` back to `NULL` (SOUL
    /// §18). A no-op-shaped update on an already-active workspace (still returns the
    /// row). Returns the updated row, or [`StoreError::NotFound`].
    pub async fn unarchive(&self, id: WorkspaceId) -> Result<Workspace> {
        let row: WorkspaceRow = sqlx::query_as(
            "UPDATE workspaces SET archived_at = NULL, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1
             RETURNING id, organisation_id, name, slug, created_at, updated_at, archived_at",
        )
        .bind(id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// **Hard-delete** a workspace (cascades to memberships, sessions,
    /// conversations). No longer reachable from the API — the org-admin archive
    /// action is a soft [`archive`](Self::archive) now (SOUL §18) — but retained for
    /// store-test cleanup and any future purge path.
    pub async fn delete(&self, id: WorkspaceId) -> Result<()> {
        let res = sqlx::query("DELETE FROM workspaces WHERE id = $1")
            .bind(id.into_uuid())
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
// Users
// ===========================================================================

/// CRUD for the `users` table. Users are global (not workspace-scoped);
/// workspace membership is modelled by [`MembershipRepo`](crate::repo::MembershipRepo).
#[derive(Clone, Debug)]
pub struct UserRepo {
    pool: PgPool,
}

/// Password-authentication row returned without interpreting the Argon2 PHC
/// string. Hashing and verification intentionally live in the API boundary.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct PasswordAccount {
    pub user_id: Uuid,
    pub workspace_id: Uuid,
    pub role: String,
    pub password_hash: String,
}

/// IDs created by the atomic first-boot transaction.
#[derive(Clone, Copy, Debug)]
pub struct BootstrapAccount {
    pub user_id: UserId,
    pub workspace_id: WorkspaceId,
}

/// Instance-local credential repository. This is global state rather than
/// workspace content, but every successful login resolves to a workspace
/// membership before a session can be issued.
#[derive(Clone, Debug)]
pub struct PasswordAuthRepo {
    pool: PgPool,
}

#[derive(Clone, Debug, sqlx::FromRow, serde::Serialize)]
pub struct LlmleafTopologyEntry {
    pub kind: String,
    pub name: String,
    pub spec: Json<serde_json::Value>,
    pub enabled: bool,
}

#[derive(Clone, Debug)]
pub struct LlmleafTopologyRepo {
    pool: PgPool,
}

impl LlmleafTopologyRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn list(&self, kind: &str, enabled_only: bool) -> Result<Vec<LlmleafTopologyEntry>> {
        sqlx::query_as(
            "SELECT kind, name, spec, enabled FROM llmleaf_topology
             WHERE kind = $1 AND ($2 = FALSE OR enabled = TRUE) ORDER BY name",
        )
        .bind(kind)
        .bind(enabled_only)
        .fetch_all(&self.pool)
        .await
        .map_err(map)
    }

    pub async fn upsert(
        &self,
        kind: &str,
        name: &str,
        spec: serde_json::Value,
        enabled: bool,
    ) -> Result<LlmleafTopologyEntry> {
        sqlx::query_as(
            "INSERT INTO llmleaf_topology (kind, name, spec, enabled) VALUES ($1, $2, $3, $4)
             ON CONFLICT (kind, name) DO UPDATE SET spec = EXCLUDED.spec,
                 enabled = EXCLUDED.enabled, updated_at = CURRENT_TIMESTAMP
             RETURNING kind, name, spec, enabled",
        )
        .bind(kind)
        .bind(name)
        .bind(Json(spec))
        .bind(enabled)
        .fetch_one(&self.pool)
        .await
        .map_err(map)
    }

    pub async fn delete(&self, kind: &str, name: &str) -> Result<()> {
        let result = sqlx::query("DELETE FROM llmleaf_topology WHERE kind = $1 AND name = $2")
            .bind(kind)
            .bind(name)
            .execute(&self.pool)
            .await
            .map_err(map)?;
        if result.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }
}

impl PasswordAuthRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// True until the singleton bootstrap row has been committed.
    pub async fn setup_required(&self) -> Result<bool> {
        let initialized: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM instance_bootstrap WHERE singleton = 1)",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(!initialized)
    }

    /// Create the first owner, home workspace and credential in one transaction.
    /// The singleton insert is last: a partial setup can never become visible.
    /// Concurrent setup requests serialize on unique keys and only one commits.
    pub async fn bootstrap(
        &self,
        email: &str,
        display_name: &str,
        password_hash: &str,
    ) -> Result<BootstrapAccount> {
        let mut tx = self.pool.begin().await.map_err(map)?;
        let already: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM instance_bootstrap WHERE singleton = 1)",
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(map)?;
        if already {
            return Err(StoreError::Conflict(
                "instance setup has already completed".to_string(),
            ));
        }

        let user_id = UserId::new();
        let workspace_id = WorkspaceId::new();
        let home_exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workspaces WHERE slug = 'home')")
                .fetch_one(&mut *tx)
                .await
                .map_err(map)?;
        let workspace_slug = if home_exists {
            format!("home-{}", &workspace_id.to_string()[..8])
        } else {
            "home".to_string()
        };
        sqlx::query("INSERT INTO users (id, email, display_name) VALUES ($1, $2, $3)")
            .bind(user_id.into_uuid())
            .bind(email)
            .bind(display_name)
            .execute(&mut *tx)
            .await
            .map_err(map)?;
        sqlx::query(
            "INSERT INTO workspaces (id, organisation_id, name, slug)
             VALUES ($1, (SELECT id FROM organisations WHERE slug = 'default'), 'Home', $2)",
        )
        .bind(workspace_id.into_uuid())
        .bind(workspace_slug)
        .execute(&mut *tx)
        .await
        .map_err(map)?;
        sqlx::query(
            "INSERT INTO memberships (workspace_id, user_id, role) VALUES ($1, $2, 'owner')",
        )
        .bind(workspace_id.into_uuid())
        .bind(user_id.into_uuid())
        .execute(&mut *tx)
        .await
        .map_err(map)?;
        sqlx::query(
            "INSERT INTO org_memberships (organisation_id, user_id, role)
             VALUES ((SELECT id FROM organisations WHERE slug = 'default'), $1, 'owner')",
        )
        .bind(user_id.into_uuid())
        .execute(&mut *tx)
        .await
        .map_err(map)?;
        sqlx::query("INSERT INTO password_credentials (user_id, password_hash) VALUES ($1, $2)")
            .bind(user_id.into_uuid())
            .bind(password_hash)
            .execute(&mut *tx)
            .await
            .map_err(map)?;
        sqlx::query("INSERT INTO instance_bootstrap (singleton, initialized_by) VALUES (1, $1)")
            .bind(user_id.into_uuid())
            .execute(&mut *tx)
            .await
            .map_err(map)?;
        tx.commit().await.map_err(map)?;
        Ok(BootstrapAccount {
            user_id,
            workspace_id,
        })
    }

    /// Resolve a local account and its first active workspace membership.
    pub async fn get_by_email(&self, email: &str) -> Result<PasswordAccount> {
        sqlx::query_as(
            "SELECT u.id AS user_id, m.workspace_id, m.role, p.password_hash
               FROM users u
               JOIN password_credentials p ON p.user_id = u.id
               JOIN memberships m ON m.user_id = u.id
               JOIN workspaces w ON w.id = m.workspace_id
              WHERE LOWER(u.email) = LOWER($1) AND w.archived_at IS NULL
              ORDER BY m.created_at ASC
              LIMIT 1",
        )
        .bind(email)
        .fetch_one(&self.pool)
        .await
        .map_err(map)
    }

    /// Set or replace a user's Argon2 PHC string (admin create/reset path).
    pub async fn set_password(&self, user_id: UserId, password_hash: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO password_credentials (user_id, password_hash) VALUES ($1, $2)
             ON CONFLICT (user_id) DO UPDATE SET password_hash = EXCLUDED.password_hash,
                 updated_at = CURRENT_TIMESTAMP",
        )
        .bind(user_id.into_uuid())
        .bind(password_hash)
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(())
    }

    /// Create a local-password user and attach it to the caller's workspace and
    /// organisation atomically. Used by the administrator user-management API.
    pub async fn create_user(
        &self,
        workspace_id: WorkspaceId,
        email: &str,
        display_name: &str,
        role: &str,
        password_hash: &str,
    ) -> Result<UserId> {
        let user_id = UserId::new();
        let mut tx = self.pool.begin().await.map_err(map)?;
        sqlx::query("INSERT INTO users (id, email, display_name) VALUES ($1, $2, $3)")
            .bind(user_id.into_uuid())
            .bind(email)
            .bind(display_name)
            .execute(&mut *tx)
            .await
            .map_err(map)?;
        sqlx::query("INSERT INTO password_credentials (user_id, password_hash) VALUES ($1, $2)")
            .bind(user_id.into_uuid())
            .bind(password_hash)
            .execute(&mut *tx)
            .await
            .map_err(map)?;
        sqlx::query("INSERT INTO memberships (workspace_id, user_id, role) VALUES ($1, $2, $3)")
            .bind(workspace_id.into_uuid())
            .bind(user_id.into_uuid())
            .bind(role)
            .execute(&mut *tx)
            .await
            .map_err(map)?;
        sqlx::query(
            "INSERT INTO org_memberships (organisation_id, user_id, role)
             SELECT organisation_id, $2, 'member' FROM workspaces WHERE id = $1
             ON CONFLICT (organisation_id, user_id) DO NOTHING",
        )
        .bind(workspace_id.into_uuid())
        .bind(user_id.into_uuid())
        .execute(&mut *tx)
        .await
        .map_err(map)?;
        tx.commit().await.map_err(map)?;
        Ok(user_id)
    }
}

impl UserRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Insert a user. `sso` is the optional `(issuer, subject)` pair.
    pub async fn create(
        &self,
        email: &str,
        display_name: &str,
        sso: Option<(&str, &str)>,
    ) -> Result<User> {
        let id = UserId::new().into_uuid();
        let (issuer, subject) = match sso {
            Some((i, s)) => (Some(i), Some(s)),
            None => (None, None),
        };
        let row: UserRow = sqlx::query_as(
            "INSERT INTO users (id, email, display_name, sso_issuer, sso_subject)
             VALUES ($1, $2, $3, $4, $5)
             RETURNING id, email, display_name, sso_issuer, sso_subject, created_at, updated_at",
        )
        .bind(id)
        .bind(email)
        .bind(display_name)
        .bind(issuer)
        .bind(subject)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch a user by id.
    pub async fn get(&self, id: UserId) -> Result<User> {
        let row: UserRow = sqlx::query_as(
            "SELECT id, email, display_name, sso_issuer, sso_subject, created_at, updated_at
             FROM users WHERE id = $1",
        )
        .bind(id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch a user by unique email.
    pub async fn get_by_email(&self, email: &str) -> Result<User> {
        let row: UserRow = sqlx::query_as(
            "SELECT id, email, display_name, sso_issuer, sso_subject, created_at, updated_at
             FROM users WHERE email = $1",
        )
        .bind(email)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch a user by email, matched **case-insensitively** (exact address, folded
    /// on both sides). Emails are stored verbatim (`email` is a plain unique `TEXT`),
    /// so an admin resolving a member by the email they typed should not have to
    /// reproduce its casing. Used by the org add-member email lookup — an exact,
    /// non-enumerating match, never a substring search (SOUL §18).
    pub async fn get_by_email_ci(&self, email: &str) -> Result<User> {
        let row: UserRow = sqlx::query_as(
            "SELECT id, email, display_name, sso_issuer, sso_subject, created_at, updated_at
             FROM users WHERE LOWER(email) = LOWER($1)",
        )
        .bind(email)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch a user by SSO `(issuer, subject)` pair, used during SSO login.
    pub async fn get_by_sso(&self, issuer: &str, subject: &str) -> Result<User> {
        let row: UserRow = sqlx::query_as(
            "SELECT id, email, display_name, sso_issuer, sso_subject, created_at, updated_at
             FROM users WHERE sso_issuer = $1 AND sso_subject = $2",
        )
        .bind(issuer)
        .bind(subject)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// List all users, by email.
    pub async fn list(&self) -> Result<Vec<User>> {
        let rows: Vec<UserRow> = sqlx::query_as(
            "SELECT id, email, display_name, sso_issuer, sso_subject, created_at, updated_at
             FROM users ORDER BY email ASC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(User::from).collect())
    }

    /// List users that are members of a given workspace.
    pub async fn list_by_workspace(&self, workspace_id: WorkspaceId) -> Result<Vec<User>> {
        let rows: Vec<UserRow> = sqlx::query_as(
            "SELECT u.id, u.email, u.display_name, u.sso_issuer, u.sso_subject,
                    u.created_at, u.updated_at
             FROM users u
             JOIN memberships m ON m.user_id = u.id
             WHERE m.workspace_id = $1
             ORDER BY u.email ASC",
        )
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(User::from).collect())
    }

    /// Update a user's display name.
    pub async fn update_display_name(&self, id: UserId, display_name: &str) -> Result<User> {
        let row: UserRow = sqlx::query_as(
            "UPDATE users SET display_name = $2, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1
             RETURNING id, email, display_name, sso_issuer, sso_subject, created_at, updated_at",
        )
        .bind(id.into_uuid())
        .bind(display_name)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Bind an SSO `(issuer, subject)` pair onto an existing user — **first-login
    /// account linking** (SOUL §18): an SSO login whose verified email matches a
    /// local/invited user adopts that user rather than creating a duplicate. The
    /// unique `(sso_issuer, sso_subject)` index means binding a subject already
    /// owned by a **different** user fails as [`StoreError::Conflict`], so a subject
    /// can never be re-pointed at two accounts.
    pub async fn bind_sso(&self, id: UserId, issuer: &str, subject: &str) -> Result<User> {
        let row: UserRow = sqlx::query_as(
            "UPDATE users SET sso_issuer = $2, sso_subject = $3, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1
             RETURNING id, email, display_name, sso_issuer, sso_subject, created_at, updated_at",
        )
        .bind(id.into_uuid())
        .bind(issuer)
        .bind(subject)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Delete a user (cascades to their memberships and sessions).
    pub async fn delete(&self, id: UserId) -> Result<()> {
        let res = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(id.into_uuid())
            .execute(&self.pool)
            .await
            .map_err(map)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }
}
