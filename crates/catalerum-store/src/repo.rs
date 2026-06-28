//! Typed CRUD repositories over the M1 tables.
//!
//! Every repository is a thin, cloneable handle holding a [`PgPool`]. All
//! tenant queries are workspace-filtered (SOUL §6.1). Queries use the
//! **runtime** sqlx API (`query`, `query_as::<_, Row>`) — never the
//! compile-time-checked macros, since there is no database at build time.

use catalerum_core::ask::{Answer, Question};
use catalerum_core::capability::{Capability, Constraints};
use catalerum_core::computer::ComputerCapabilities;
use catalerum_core::{
    id::{
        AgentProfileId, AutomationId, AutomationRunId, AutomationStepId, BoardId, BucketId,
        CalendarId, ChunkId, ColumnId, ComputerAgentId, ConnectionId, ConversationId, DocumentId,
        EmailId, EventId, GrantId, LinkId, MailboxId, McpEndpointId, McpServerId, MemoryId,
        MessageId, NoteId, ObjectId, ObjectLabelId, OrganisationId, PendingApprovalId,
        PendingQuestionId, SkillId, TaskId, TerminalSessionId, UiDefinitionId, UserId, WorkspaceId,
    },
    model::{
        AgentProfile, ApprovalDecision, Attachment, Author, Automation, AutomationRun,
        AutomationStep, Board, Bucket, Calendar, Chunk, Code, Connection, ConnectionKind,
        Conversation, CreationPolicy, Cursor, Document, Email, EntityRef, Event, ExecutorKind,
        Grant, Link, LlmSettings, Mailbox, Map, McpEndpoint, McpServerDef, Membership, Memory,
        MemoryScope, Message, MessageRole, Note, ObjectLabel, OrgMembership, OrgRole, Organisation,
        Origin, PendingApproval, PendingQuestion, Profile, Role, RunStatus, SandboxState,
        SearchSettings, Skill, SourceRef, StepStatus, StorageSettings, StoredObject, Task,
        TaskStatus, TerminalSession, TerminalSessionStatus, ToolGuard, UiDefinition, User,
        Workspace, WorkspaceSandboxRecord,
    },
    model_ui::UiSpec,
};
use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::types::Json;
// Kept as a local alias to avoid obscuring the repository code with backend
// conditionals. Its concrete type is chosen at compile time in `pool`.
use crate::DbPool as PgPool;
use uuid::Uuid;

use crate::error::{Result, StoreError};
use crate::rows::{
    author_to_parts, board_from_parts, computer_platform_to_text, connection_kind_to_text,
    creation_policy_to_text, memory_scope_to_text, message_role_to_text, org_role_to_text,
    origin_to_text, role_to_text, run_status_to_text, sandbox_state_to_text, source_to_parts,
    step_status_to_text, task_status_to_text, terminal_session_status_to_text, AgentProfileRow,
    AppDataRow, AutomationRow, AutomationRunRow, AutomationStepRow, BoardRow, BucketRow,
    CalendarRow, ChunkRow, ColumnRow, ComputerAgent, ComputerAgentRow, ConnectionRow,
    ConversationRow, DocumentRow, EmailRow, EventRow, GrantRow, JobRow, JobStatus, LinkRow,
    LlmSettingsRow, LoginToken, MailboxRow, McpEndpointRow, McpServerRow, MembershipRow, MemoryRow,
    MessageRow, MessageSearchRow, NoteRow, ObjectLabelRow, ObjectRow, OrgMembershipRow,
    OrganisationRow, PendingApprovalRow, PendingQuestionRow, ProfileRow, SearchSettingsRow,
    Session, SkillRow, StorageSettingsRow, TaskRow, TerminalSessionRow, UiDefinitionRow, UserRow,
    WorkspaceRow, WorkspaceSandboxRow,
};

/// Map any `sqlx::Error` arising from a query into a classified [`StoreError`].
fn map(err: sqlx::Error) -> StoreError {
    StoreError::from_sqlx(err)
}

/// Convert a relative timeout to an absolute cutoff before binding it. Keeping
/// interval arithmetic in Rust makes lease queries native on both PostgreSQL
/// and SQLite (which deliberately has no `make_interval`).
fn cutoff_before(timeout: Duration) -> DateTime<Utc> {
    let delta = chrono::Duration::from_std(timeout).unwrap_or(chrono::Duration::MAX);
    Utc::now()
        .checked_sub_signed(delta)
        .unwrap_or(DateTime::<Utc>::MIN_UTC)
}

/// Slug of the well-known **default organisation** (SOUL §18). Seeded by the
/// `0046` migration; the org-less [`WorkspaceRepo::create`] convenience attaches
/// new workspaces to it. Mirrors `catalerum_iam::DEFAULT_ORGANISATION_SLUG`.
pub const DEFAULT_ORGANISATION_SLUG: &str = "default";

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
/// workspace membership is modelled by [`MembershipRepo`].
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

// ===========================================================================
// Sessions
// ===========================================================================

/// CRUD for the `sessions` table (opaque server-side auth sessions).
#[derive(Clone, Debug)]
pub struct SessionRepo {
    pool: PgPool,
}

impl SessionRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create a session for `(workspace, user)`. `token_hash` must already be a
    /// hash of the raw token (the store never sees plaintext tokens). `grant_id`
    /// scopes the token to a named §19 grant (SOUL §19/§26) — its same-workspace
    /// composite FK rejects a cross-workspace/unknown grant at write time and
    /// cascade-revokes the session if the grant is later deleted; `None` is a
    /// role-derived session.
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        user_id: UserId,
        token_hash: &str,
        grant_id: Option<GrantId>,
        expires_at: DateTime<Utc>,
    ) -> Result<Session> {
        let id = Uuid::new_v4();
        let row: Session = sqlx::query_as(
            "INSERT INTO sessions (id, workspace_id, user_id, token_hash, grant_id, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6)
             RETURNING id, workspace_id, user_id, token_hash, grant_id, created_at, expires_at",
        )
        .bind(id)
        .bind(workspace_id.into_uuid())
        .bind(user_id.into_uuid())
        .bind(token_hash)
        .bind(grant_id.map(GrantId::into_uuid))
        .bind(expires_at)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row)
    }

    /// Fetch a session by id.
    pub async fn get(&self, id: Uuid) -> Result<Session> {
        let row: Session = sqlx::query_as(
            "SELECT id, workspace_id, user_id, token_hash, grant_id, created_at, expires_at
             FROM sessions WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row)
    }

    /// Look up a session by its `token_hash` (the auth hot path). Expired
    /// sessions are excluded.
    pub async fn get_by_token_hash(&self, token_hash: &str) -> Result<Session> {
        let row: Session = sqlx::query_as(
            "SELECT id, workspace_id, user_id, token_hash, grant_id, created_at, expires_at
             FROM sessions WHERE token_hash = $1 AND expires_at > CURRENT_TIMESTAMP",
        )
        .bind(token_hash)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row)
    }

    /// List a user's active (non-expired) sessions.
    pub async fn list_by_user(&self, user_id: UserId) -> Result<Vec<Session>> {
        let rows: Vec<Session> = sqlx::query_as(
            "SELECT id, workspace_id, user_id, token_hash, grant_id, created_at, expires_at
             FROM sessions WHERE user_id = $1 AND expires_at > CURRENT_TIMESTAMP
             ORDER BY created_at DESC",
        )
        .bind(user_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows)
    }

    /// Revoke a single session by id.
    pub async fn delete(&self, id: Uuid) -> Result<()> {
        let res = sqlx::query("DELETE FROM sessions WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(map)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// Garbage-collect expired sessions. Returns the number deleted.
    pub async fn delete_expired(&self) -> Result<u64> {
        let res = sqlx::query("DELETE FROM sessions WHERE expires_at <= CURRENT_TIMESTAMP")
            .execute(&self.pool)
            .await
            .map_err(map)?;
        Ok(res.rows_affected())
    }
}

// ===========================================================================
// Login tokens
// ===========================================================================

/// CRUD for the `login_tokens` table (one-time magic-link tokens, SOUL §18).
///
/// The store only ever sees a `token_hash` — a hash of the raw token handed to
/// the caller. Tokens are consumed exactly once via [`Self::consume`], which
/// atomically flips `consumed_at` only if it is still `NULL`.
#[derive(Clone, Debug)]
pub struct LoginTokenRepo {
    pool: PgPool,
}

impl LoginTokenRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create a one-time login token for `(workspace, user)`. `token_hash` must
    /// already be a hash of the raw token (the store never sees plaintext).
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        user_id: UserId,
        token_hash: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<LoginToken> {
        let row: LoginToken = sqlx::query_as(
            "INSERT INTO login_tokens (token_hash, workspace_id, user_id, expires_at)
             VALUES ($1, $2, $3, $4)
             RETURNING token_hash, workspace_id, user_id, created_at, expires_at, consumed_at",
        )
        .bind(token_hash)
        .bind(workspace_id.into_uuid())
        .bind(user_id.into_uuid())
        .bind(expires_at)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row)
    }

    /// Look up a login token by its `token_hash`, or [`StoreError::NotFound`].
    /// Returns the row regardless of consumption/expiry so callers can
    /// disambiguate "unknown" from "already consumed" / "expired".
    pub async fn get_by_token_hash(&self, token_hash: &str) -> Result<LoginToken> {
        let row: LoginToken = sqlx::query_as(
            "SELECT token_hash, workspace_id, user_id, created_at, expires_at, consumed_at
             FROM login_tokens WHERE token_hash = $1",
        )
        .bind(token_hash)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row)
    }

    /// Atomically consume a login token: flip `consumed_at` to `consumed_at`
    /// only if it was still `NULL`. Returns the row as it was **before**
    /// consumption (i.e. `consumed_at` is `NULL` in the returned value).
    ///
    /// Returns [`StoreError::NotFound`] if the token is unknown **or** was
    /// already consumed — the caller can distinguish the two by following up
    /// with [`Self::get_by_token_hash`].
    pub async fn consume(
        &self,
        token_hash: &str,
        consumed_at: DateTime<Utc>,
    ) -> Result<LoginToken> {
        #[cfg(not(feature = "sqlite"))]
        let statement = "UPDATE login_tokens SET consumed_at = $2
             WHERE token_hash = $1 AND consumed_at IS NULL
             RETURNING token_hash, workspace_id, user_id, created_at, expires_at,
                       NULL::timestamptz AS consumed_at";
        #[cfg(feature = "sqlite")]
        let statement = "UPDATE login_tokens SET consumed_at = $2
             WHERE token_hash = $1 AND consumed_at IS NULL
             RETURNING token_hash, workspace_id, user_id, created_at, expires_at,
                       NULL AS consumed_at";
        let row: LoginToken = sqlx::query_as(statement)
            .bind(token_hash)
            .bind(consumed_at)
            .fetch_one(&self.pool)
            .await
            .map_err(map)?;
        Ok(row)
    }

    /// Garbage-collect expired (and not-yet-consumed) tokens. Returns the count.
    pub async fn delete_expired(&self) -> Result<u64> {
        let res = sqlx::query("DELETE FROM login_tokens WHERE expires_at <= CURRENT_TIMESTAMP")
            .execute(&self.pool)
            .await
            .map_err(map)?;
        Ok(res.rows_affected())
    }
}

// ===========================================================================
// Conversations
// ===========================================================================

/// CRUD for the `conversations` table.
#[derive(Clone, Debug)]
pub struct ConversationRepo {
    pool: PgPool,
}

impl ConversationRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create a conversation in a workspace.
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        title: Option<&str>,
        origin: Origin,
    ) -> Result<Conversation> {
        self.create_with_id(workspace_id, ConversationId::new(), title, origin)
            .await
    }

    /// Create a conversation with a caller-chosen id, idempotently.
    ///
    /// The web chat uses this for its durable outbox: if the POST response is lost,
    /// retrying the same request returns the row that was already committed instead
    /// of creating a second empty conversation. An id owned by another workspace is
    /// never returned (the scoped re-read yields [`StoreError::NotFound`]).
    pub async fn create_with_id(
        &self,
        workspace_id: WorkspaceId,
        id: ConversationId,
        title: Option<&str>,
        origin: Origin,
    ) -> Result<Conversation> {
        let origin_text = origin_to_text(origin)?;
        let inserted: Option<ConversationRow> = sqlx::query_as(
            "INSERT INTO conversations (id, workspace_id, title, origin)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (id) DO NOTHING
             RETURNING id, workspace_id, title, tags, title_manual, origin, agent_profile_id, model, reasoning_effort, summary, summary_upto, created_at, updated_at",
        )
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(title)
        .bind(origin_text)
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        match inserted {
            Some(row) => row.try_into(),
            None => self.get(workspace_id, id).await,
        }
    }

    /// Atomically create a conversation and its first plain-text message.
    ///
    /// Automation output uses this so a failed message insert can never leave an
    /// empty thread in the chat sidebar. The caller chooses the origin and role;
    /// ids and timestamps are assigned here.
    pub async fn create_with_initial_message(
        &self,
        workspace_id: WorkspaceId,
        title: Option<&str>,
        origin: Origin,
        role: MessageRole,
        content: &str,
    ) -> Result<(Conversation, Message)> {
        let conversation_id = ConversationId::new();
        let message_id = MessageId::new();
        let origin_text = origin_to_text(origin)?;
        let role_text = message_role_to_text(role)?;
        let mut tx = self.pool.begin().await.map_err(map)?;

        let conversation_row: ConversationRow = sqlx::query_as(
            "INSERT INTO conversations (id, workspace_id, title, origin)
             VALUES ($1, $2, $3, $4)
             RETURNING id, workspace_id, title, tags, title_manual, origin, agent_profile_id, model, reasoning_effort, summary, summary_upto, created_at, updated_at",
        )
        .bind(conversation_id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(title)
        .bind(origin_text)
        .fetch_one(&mut *tx)
        .await
        .map_err(map)?;

        let message_row: MessageRow = sqlx::query_as(
            "INSERT INTO messages (id, conversation_id, role, content)
             VALUES ($1, $2, $3, $4)
             RETURNING id, conversation_id, role, content, attachments, skill, tool_calls,
                       tool_call_id, tool_is_error, tool_duration_ms, prompt_tokens,
                       completion_tokens, total_tokens, cached_tokens, cache_creation_tokens,
                       cost_usd, created_at",
        )
        .bind(message_id.into_uuid())
        .bind(conversation_id.into_uuid())
        .bind(role_text)
        .bind(content)
        .fetch_one(&mut *tx)
        .await
        .map_err(map)?;

        let conversation = conversation_row.try_into()?;
        let message = message_row.try_into()?;
        tx.commit().await.map_err(map)?;
        Ok((conversation, message))
    }

    /// Fetch a conversation, scoped to its workspace.
    pub async fn get(&self, workspace_id: WorkspaceId, id: ConversationId) -> Result<Conversation> {
        let row: ConversationRow = sqlx::query_as(
            "SELECT id, workspace_id, title, tags, title_manual, origin, agent_profile_id, model, reasoning_effort, summary, summary_upto, created_at, updated_at
             FROM conversations WHERE id = $1 AND workspace_id = $2",
        )
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// List a workspace's conversations, newest first.
    pub async fn list_by_workspace(&self, workspace_id: WorkspaceId) -> Result<Vec<Conversation>> {
        let rows: Vec<ConversationRow> = sqlx::query_as(
            "SELECT id, workspace_id, title, tags, title_manual, origin, agent_profile_id, model, reasoning_effort, summary, summary_upto, created_at, updated_at
             FROM conversations WHERE workspace_id = $1
             ORDER BY created_at DESC",
        )
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(Conversation::try_from).collect()
    }

    /// Rename a conversation (workspace-scoped). Pass `None` to clear the title.
    pub async fn update_title(
        &self,
        workspace_id: WorkspaceId,
        id: ConversationId,
        title: Option<&str>,
    ) -> Result<Conversation> {
        let row: ConversationRow = sqlx::query_as(
            "UPDATE conversations SET title = $3, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2
             RETURNING id, workspace_id, title, tags, title_manual, origin, agent_profile_id, model, reasoning_effort, summary, summary_upto, created_at, updated_at",
        )
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(title)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Bind (or unbind, with `None`) the [`AgentProfile`] a conversation runs as —
    /// the chat "run as a profile" picker (SOUL §19/§12), workspace-scoped.
    /// [`StoreError::NotFound`] if the conversation is absent. The same-workspace FK
    /// guarantees the profile (when `Some`) belongs to this workspace.
    pub async fn set_agent_profile(
        &self,
        workspace_id: WorkspaceId,
        id: ConversationId,
        agent_profile_id: Option<AgentProfileId>,
    ) -> Result<Conversation> {
        let row: ConversationRow = sqlx::query_as(
            "UPDATE conversations SET agent_profile_id = $3, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2
             RETURNING id, workspace_id, title, tags, title_manual, origin, agent_profile_id, model, reasoning_effort, summary, summary_upto, created_at, updated_at",
        )
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(agent_profile_id.map(AgentProfileId::into_uuid))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Pin (or clear, with `None`) the model a conversation's chat loop thinks with
    /// — the chat "model" picker (SOUL §7/§12), workspace-scoped.
    /// [`StoreError::NotFound`] if the conversation is absent. The model id is a
    /// free-form gateway string (validated, if at all, at the app layer), mirroring
    /// the user's `llm_settings.chat_model`; pass a trimmed non-empty value or
    /// `None` to clear.
    pub async fn set_model(
        &self,
        workspace_id: WorkspaceId,
        id: ConversationId,
        model: Option<&str>,
    ) -> Result<Conversation> {
        let row: ConversationRow = sqlx::query_as(
            "UPDATE conversations SET model = $3, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2
             RETURNING id, workspace_id, title, tags, title_manual, origin, agent_profile_id, model, reasoning_effort, summary, summary_upto, created_at, updated_at",
        )
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(model)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Set (or clear, with `None`) the reasoning ("thinking") effort a conversation's
    /// chat loop requests — the chat "thinking" picker (SOUL §7/§12), workspace-scoped.
    /// [`StoreError::NotFound`] if the conversation is absent. The effort is a
    /// free-form gateway token (`low`/`medium`/`high`/`xhigh`/`max`, passed through to
    /// the model, validated only at the app layer); pass a trimmed non-empty value or
    /// `None` to clear (no reasoning requested).
    pub async fn set_reasoning(
        &self,
        workspace_id: WorkspaceId,
        id: ConversationId,
        reasoning_effort: Option<&str>,
    ) -> Result<Conversation> {
        let row: ConversationRow = sqlx::query_as(
            "UPDATE conversations SET reasoning_effort = $3, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2
             RETURNING id, workspace_id, title, tags, title_manual, origin, agent_profile_id, model, reasoning_effort, summary, summary_upto, created_at, updated_at",
        )
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(reasoning_effort)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Set (or clear, with `None`) a conversation's rolling auto-compaction
    /// summary + the last message it covers (SOUL §7/§12), workspace-scoped.
    /// The two columns move together — a summary without its coverage anchor is
    /// meaningless (and the seed ignores a half-set pair). The FK's
    /// `ON DELETE SET NULL` clears the anchor when a regenerate prunes the row
    /// it points at, which invalidates the summary the same way.
    /// [`StoreError::NotFound`] if the conversation is absent.
    pub async fn set_summary(
        &self,
        workspace_id: WorkspaceId,
        id: ConversationId,
        summary: Option<(&str, MessageId)>,
    ) -> Result<()> {
        let (text, upto) = match summary {
            Some((text, upto)) => (Some(text), Some(upto.into_uuid())),
            None => (None, None),
        };
        let res = sqlx::query(
            "UPDATE conversations SET summary = $3, summary_upto = $4, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2",
        )
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(text)
        .bind(upto)
        .execute(&self.pool)
        .await
        .map_err(map)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// Store the background auto-title/auto-tag pass result (workspace-scoped):
    /// `tags` always; `title` only when it is `Some` **and** the thread has no
    /// user-chosen title (`title_manual` — an explicit rename pins the name and
    /// the generator must never overwrite it). `StoreError::NotFound` if absent.
    pub async fn set_generated_meta(
        &self,
        workspace_id: WorkspaceId,
        id: ConversationId,
        title: Option<&str>,
        tags: &[String],
    ) -> Result<()> {
        let res = sqlx::query(
            "UPDATE conversations
             SET tags = $3,
                 title = COALESCE(CASE WHEN NOT title_manual THEN $4 END, title),
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2",
        )
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(Json(tags.to_vec()))
        .bind(title)
        .execute(&self.pool)
        .await
        .map_err(map)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// Rename a conversation (workspace-scoped, **manual** — sets `title_manual`
    /// so the background auto-title pass stops overwriting the chosen name).
    /// Pass `None` to clear the title (and unpin).
    pub async fn rename_manual(
        &self,
        workspace_id: WorkspaceId,
        id: ConversationId,
        title: Option<&str>,
    ) -> Result<()> {
        let res = sqlx::query(
            "UPDATE conversations
             SET title = $3, title_manual = ($3 IS NOT NULL), updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2",
        )
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(title)
        .execute(&self.pool)
        .await
        .map_err(map)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// Delete a conversation (cascades to its messages), workspace-scoped.
    pub async fn delete(&self, workspace_id: WorkspaceId, id: ConversationId) -> Result<()> {
        let res = sqlx::query("DELETE FROM conversations WHERE id = $1 AND workspace_id = $2")
            .bind(id.into_uuid())
            .bind(workspace_id.into_uuid())
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
// Messages
// ===========================================================================

/// A message to insert. The id and `created_at` are assigned by the store.
#[derive(Clone, Debug)]
pub struct NewMessage<'a> {
    /// Owning conversation.
    pub conversation_id: ConversationId,
    /// Pre-chosen row id. `None` mints a fresh [`MessageId`] at insert time (the
    /// default); `Some` lets a caller fix the id ahead of the insert — the chat
    /// path pre-generates the anchoring user message's id so a detached turn's
    /// Valkey stream key (`cat:turnbuf:{conv}:{msg}`) is known synchronously,
    /// before the row persists (SOUL §7/§12). `created_at` stays DB-assigned.
    pub id: Option<MessageId>,
    /// Message role.
    pub role: MessageRole,
    /// Text content (may be empty for a pure tool-call assistant turn).
    pub content: &'a str,
    /// User-turn file/image references (SOUL §9/§12); empty for non-user rows.
    pub attachments: &'a [catalerum_core::model::Attachment],
    /// A user turn's `/<skill>` invocation snapshot (SOUL §12/§23); `None` for
    /// every other row.
    pub skill: Option<&'a catalerum_core::model::SkillInvocation>,
    /// Tool calls emitted by an assistant turn (empty otherwise).
    pub tool_calls: &'a [catalerum_core::model::ToolCall],
    /// For a `Tool` message, the id of the call it answers.
    pub tool_call_id: Option<&'a str>,
    /// For a `Tool` message, whether the call failed (the `content` holds the
    /// error payload). `false` for non-tool rows.
    pub tool_is_error: bool,
    /// For a `Tool` message, the dispatch duration in milliseconds, when measured.
    pub tool_duration_ms: Option<i64>,
    /// Per-turn token + cost accounting for the exchange — set only on the
    /// **final assistant message** (the agent loop's summed usage), `None`
    /// everywhere else. Persisted so a replayed transcript keeps the token
    /// info-icon / cost readout the live turn showed.
    pub usage: Option<catalerum_core::stream::Usage>,
}

impl<'a> NewMessage<'a> {
    /// A plain text message with no tool calls.
    #[must_use]
    pub fn text(conversation_id: ConversationId, role: MessageRole, content: &'a str) -> Self {
        Self {
            conversation_id,
            id: None,
            role,
            content,
            attachments: &[],
            skill: None,
            tool_calls: &[],
            tool_call_id: None,
            tool_is_error: false,
            tool_duration_ms: None,
            usage: None,
        }
    }
}

/// Default cap on [`MessageRepo::search_in_workspace`] results.
pub const DEFAULT_MESSAGE_SEARCH_LIMIT: i64 = 50;

/// A message-content search hit: the matched [`Message`] plus the title of the
/// conversation it belongs to (for displaying / opening the thread).
#[derive(Clone, Debug)]
pub struct MessageSearchHit {
    pub message: Message,
    pub conversation_title: Option<String>,
}

/// CRUD for the `messages` table.
#[derive(Clone, Debug)]
pub struct MessageRepo {
    pool: PgPool,
}

impl MessageRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Insert a message and return the stored row.
    pub async fn insert(&self, msg: &NewMessage<'_>) -> Result<Message> {
        let id = msg.id.unwrap_or_default().into_uuid();
        let role_text = message_role_to_text(msg.role)?;
        // Token counts are u32 on the wire; widen to BIGINT for storage. Bound
        // together with the message so usage is atomic — all six or all NULL.
        let u = msg.usage.as_ref();
        let row: MessageRow = sqlx::query_as(
            "INSERT INTO messages \
             (id, conversation_id, role, content, tool_calls, tool_call_id, tool_is_error, \
              tool_duration_ms, prompt_tokens, completion_tokens, total_tokens, cached_tokens, \
              cache_creation_tokens, cost_usd, attachments, skill)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
             RETURNING id, conversation_id, role, content, attachments, skill, tool_calls, \
                       tool_call_id, tool_is_error, tool_duration_ms, prompt_tokens, \
                       completion_tokens, total_tokens, cached_tokens, cache_creation_tokens, \
                       cost_usd, created_at",
        )
        .bind(id)
        .bind(msg.conversation_id.into_uuid())
        .bind(role_text)
        .bind(msg.content)
        .bind(Json(msg.tool_calls.to_vec()))
        .bind(msg.tool_call_id)
        .bind(msg.tool_is_error)
        .bind(msg.tool_duration_ms)
        .bind(u.map(|u| i64::from(u.prompt_tokens)))
        .bind(u.map(|u| i64::from(u.completion_tokens)))
        .bind(u.map(|u| i64::from(u.total_tokens)))
        .bind(u.map(|u| i64::from(u.cached_tokens)))
        .bind(u.map(|u| i64::from(u.cache_creation_tokens)))
        .bind(u.and_then(|u| u.cost_usd))
        .bind(Json(msg.attachments.to_vec()))
        .bind(msg.skill.map(Json))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Stamp the summed token/cost [`Usage`](catalerum_core::stream::Usage) of a
    /// finished exchange onto its **final assistant message** (SOUL §12).
    ///
    /// The agent loop persists each turn incrementally as it completes, with no
    /// usage — the exchange total isn't known until the loop ends. This back-fills
    /// that total onto the last assistant row once the run finishes, so a reopened
    /// conversation replays the same token info-icon / cost readout the live turn
    /// showed. A `None` usage writes NULLs (a no-op on an already-null row).
    /// Returns [`StoreError::NotFound`] if `id` matches no row.
    pub async fn set_usage(
        &self,
        id: MessageId,
        usage: Option<catalerum_core::stream::Usage>,
    ) -> Result<()> {
        let u = usage.as_ref();
        let res = sqlx::query(
            "UPDATE messages SET \
             prompt_tokens = $2, completion_tokens = $3, total_tokens = $4, \
             cached_tokens = $5, cache_creation_tokens = $6, cost_usd = $7 \
             WHERE id = $1",
        )
        .bind(id.into_uuid())
        .bind(u.map(|u| i64::from(u.prompt_tokens)))
        .bind(u.map(|u| i64::from(u.completion_tokens)))
        .bind(u.map(|u| i64::from(u.total_tokens)))
        .bind(u.map(|u| i64::from(u.cached_tokens)))
        .bind(u.map(|u| i64::from(u.cache_creation_tokens)))
        .bind(u.and_then(|u| u.cost_usd))
        .execute(&self.pool)
        .await
        .map_err(map)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// Fetch a single message by id.
    pub async fn get(&self, id: MessageId) -> Result<Message> {
        let row: MessageRow = sqlx::query_as(
            "SELECT id, conversation_id, role, content, attachments, skill, tool_calls, tool_call_id, tool_is_error, tool_duration_ms, prompt_tokens, completion_tokens, total_tokens, cached_tokens, cache_creation_tokens, cost_usd, created_at
             FROM messages WHERE id = $1",
        )
        .bind(id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// List all messages in a conversation, oldest first (LLM replay order).
    pub async fn list_by_conversation(
        &self,
        conversation_id: ConversationId,
    ) -> Result<Vec<Message>> {
        let rows: Vec<MessageRow> = sqlx::query_as(
            "SELECT id, conversation_id, role, content, attachments, skill, tool_calls, tool_call_id, tool_is_error, tool_duration_ms, prompt_tokens, completion_tokens, total_tokens, cached_tokens, cache_creation_tokens, cost_usd, created_at
             FROM messages WHERE conversation_id = $1
             ORDER BY created_at ASC, id ASC",
        )
        .bind(conversation_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(Message::try_from).collect()
    }

    /// List the most recent `limit` messages in a conversation, oldest first
    /// (a bounded context window for the agent loop).
    pub async fn list_recent(
        &self,
        conversation_id: ConversationId,
        limit: i64,
    ) -> Result<Vec<Message>> {
        let rows: Vec<MessageRow> = sqlx::query_as(
            "SELECT id, conversation_id, role, content, attachments, skill, tool_calls, tool_call_id, tool_is_error, tool_duration_ms, prompt_tokens, completion_tokens, total_tokens, cached_tokens, cache_creation_tokens, cost_usd, created_at
             FROM (
                 SELECT id, conversation_id, role, content, attachments, skill, tool_calls, tool_call_id, tool_is_error, tool_duration_ms, prompt_tokens, completion_tokens, total_tokens, cached_tokens, cache_creation_tokens, cost_usd, created_at
                 FROM messages WHERE conversation_id = $1
                 ORDER BY created_at DESC, id DESC
                 LIMIT $2
             ) recent
             ORDER BY created_at ASC, id ASC",
        )
        .bind(conversation_id.into_uuid())
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(Message::try_from).collect()
    }

    /// List the most recent `limit` messages **strictly after** `anchor` in
    /// replay order (`created_at ASC, id ASC`), oldest first — the bounded seed
    /// window for a conversation whose older prefix was folded into a rolling
    /// compaction summary (SOUL §7/§12): the summary stands in for everything
    /// up to and including the anchor, this returns what comes after. An
    /// unknown anchor matches the empty subquery and returns nothing (callers
    /// only pass a `summary_upto` the FK guarantees exists).
    pub async fn list_recent_after(
        &self,
        conversation_id: ConversationId,
        anchor: MessageId,
        limit: i64,
    ) -> Result<Vec<Message>> {
        let rows: Vec<MessageRow> = sqlx::query_as(
            "SELECT id, conversation_id, role, content, attachments, skill, tool_calls, tool_call_id, tool_is_error, tool_duration_ms, prompt_tokens, completion_tokens, total_tokens, cached_tokens, cache_creation_tokens, cost_usd, created_at
             FROM (
                 SELECT id, conversation_id, role, content, attachments, skill, tool_calls, tool_call_id, tool_is_error, tool_duration_ms, prompt_tokens, completion_tokens, total_tokens, cached_tokens, cache_creation_tokens, cost_usd, created_at
                 FROM messages
                 WHERE conversation_id = $1
                   AND (created_at, id) > (
                       SELECT created_at, id FROM messages WHERE id = $2
                   )
                 ORDER BY created_at DESC, id DESC
                 LIMIT $3
             ) recent
             ORDER BY created_at ASC, id ASC",
        )
        .bind(conversation_id.into_uuid())
        .bind(anchor.into_uuid())
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(Message::try_from).collect()
    }

    /// Search message **content** across a workspace, newest match first. Joins
    /// each `messages` row to its conversation (for workspace scoping + the thread
    /// title) and matches `query` as a **literal, case-insensitive substring**
    /// (`strpos(lower(content), lower($2))` — no `LIKE` wildcard semantics, so a
    /// user's `%`/`_` are matched literally). A blank `query` returns nothing
    /// (the caller should not run an unbounded "match everything" search). Bounded
    /// by `limit` (floored at 1); the scan is unindexed but bounded by human-scale
    /// message volume + the limit.
    pub async fn search_in_workspace(
        &self,
        workspace_id: WorkspaceId,
        query: &str,
        limit: i64,
    ) -> Result<Vec<MessageSearchHit>> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<MessageSearchRow> = sqlx::query_as(
            "SELECT m.id, m.conversation_id, m.role, m.content, m.tool_calls, m.tool_call_id,
                    m.tool_is_error, m.tool_duration_ms, m.created_at, c.title AS conversation_title
             FROM messages m
             JOIN conversations c ON c.id = m.conversation_id
             WHERE c.workspace_id = $1 AND strpos(lower(m.content), lower($2)) > 0
             ORDER BY m.created_at DESC, m.id DESC
             LIMIT $3",
        )
        .bind(workspace_id.into_uuid())
        .bind(query)
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter()
            .map(|row| {
                let (msg_row, conversation_title) = row.split();
                Message::try_from(msg_row).map(|message| MessageSearchHit {
                    message,
                    conversation_title,
                })
            })
            .collect()
    }

    /// Delete a single message.
    pub async fn delete(&self, id: MessageId) -> Result<()> {
        let res = sqlx::query("DELETE FROM messages WHERE id = $1")
            .bind(id.into_uuid())
            .execute(&self.pool)
            .await
            .map_err(map)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// Delete every message in `conversation_id` that comes *after* `anchor` in
    /// replay order (`created_at ASC, id ASC`) — the anchor itself is kept.
    ///
    /// Used to regenerate a turn (SOUL §12): the anchoring user message stays and
    /// the transcript tail it produced (the old answer + any later exchanges) is
    /// dropped before the agent loop re-answers it. The `(created_at, id)`
    /// row-value comparison is exactly the replay ordering, so "everything after
    /// the anchor" is expressed precisely. Returns the number of rows removed
    /// (`0` when the anchor was already the last message, or unknown — not an
    /// error). The anchor is *not* required to exist: an unknown id matches the
    /// empty subquery and deletes nothing.
    pub async fn delete_after(
        &self,
        conversation_id: ConversationId,
        anchor: MessageId,
    ) -> Result<u64> {
        let res = sqlx::query(
            "DELETE FROM messages \
             WHERE conversation_id = $1 \
               AND (created_at, id) > ( \
                   SELECT created_at, id FROM messages WHERE id = $2 \
               )",
        )
        .bind(conversation_id.into_uuid())
        .bind(anchor.into_uuid())
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(res.rows_affected())
    }
}

// ===========================================================================
// Notes
// ===========================================================================

/// The full set of `notes` columns, in row order (for `SELECT`/`RETURNING`).
const NOTE_COLS: &str = "id, workspace_id, author_kind, author_id, title, markdown, tags, \
     created_at, updated_at";

/// Default cap on [`NoteRepo::list_by_workspace`] (SOUL §18 — every read is
/// bounded, so a workspace with thousands of notes can't balloon the agent context
/// or the API payload). The list is bounded to this many (most-recently-edited
/// first); normal note collections fall well under it and are unaffected. Mirrors
/// [`DEFAULT_OBJECT_LIMIT`].
pub const DEFAULT_NOTE_LIMIT: i64 = 1000;

/// CRUD for the `notes` table — user- or LLM-authored markdown notes (SOUL §21).
/// Every query is workspace-filtered (SOUL §6.1/§18); notes list
/// most-recently-edited first.
#[derive(Clone, Debug)]
pub struct NoteRepo {
    pool: PgPool,
}

impl NoteRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create a note in a workspace, authored by `author` (a user or an agent,
    /// SOUL §21). Returns the stored note.
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        author: Author,
        title: &str,
        markdown: &str,
        tags: &[String],
    ) -> Result<Note> {
        let id = NoteId::new().into_uuid();
        let (author_kind, author_id) = author_to_parts(author);
        let row: NoteRow = sqlx::query_as(&format!(
            "INSERT INTO notes
                 (id, workspace_id, author_kind, author_id, title, markdown, tags)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             RETURNING {NOTE_COLS}"
        ))
        .bind(id)
        .bind(workspace_id.into_uuid())
        .bind(author_kind)
        .bind(author_id)
        .bind(title)
        .bind(markdown)
        .bind(Json(tags.to_vec()))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Fetch a note, scoped to its workspace.
    pub async fn get(&self, workspace_id: WorkspaceId, id: NoteId) -> Result<Note> {
        let row: NoteRow = sqlx::query_as(&format!(
            "SELECT {NOTE_COLS} FROM notes
             WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// List a workspace's notes, most-recently-edited first, bounded to `limit`
    /// rows (floored at 1, see [`DEFAULT_NOTE_LIMIT`]) so a large note collection
    /// can't return an unbounded set (SOUL §18). The bound applies after the
    /// most-recent ordering, so you get the newest `limit` notes.
    pub async fn list_by_workspace(
        &self,
        workspace_id: WorkspaceId,
        limit: i64,
    ) -> Result<Vec<Note>> {
        let rows: Vec<NoteRow> = sqlx::query_as(&format!(
            "SELECT {NOTE_COLS} FROM notes
             WHERE workspace_id = $1
             ORDER BY updated_at DESC, id ASC
             LIMIT $2"
        ))
        .bind(workspace_id.into_uuid())
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(Note::try_from).collect()
    }

    /// Update a note's `title`, `markdown`, and `tags` (workspace-scoped),
    /// bumping `updated_at`. The author is immutable. Returns the updated note,
    /// or [`StoreError::NotFound`] if no such note exists in the workspace.
    pub async fn update(
        &self,
        workspace_id: WorkspaceId,
        id: NoteId,
        title: &str,
        markdown: &str,
        tags: &[String],
    ) -> Result<Note> {
        let row: NoteRow = sqlx::query_as(&format!(
            "UPDATE notes SET title = $3, markdown = $4, tags = $5, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2
             RETURNING {NOTE_COLS}"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(title)
        .bind(markdown)
        .bind(Json(tags.to_vec()))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Delete a note, workspace-scoped. [`StoreError::NotFound`] if absent.
    pub async fn delete(&self, workspace_id: WorkspaceId, id: NoteId) -> Result<()> {
        let res = sqlx::query("DELETE FROM notes WHERE id = $1 AND workspace_id = $2")
            .bind(id.into_uuid())
            .bind(workspace_id.into_uuid())
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
// Links — relationships between objects (SOUL §5/§6.3)
// ===========================================================================

const LINK_COLS: &str = "id, workspace_id, from_kind, from_id, to_kind, to_id, label, note, \
     author_kind, author_id, created_at, updated_at";

/// Default cap on [`LinkRepo`] list reads (SOUL §18 — every read is bounded).
/// Mirrors [`DEFAULT_NOTE_LIMIT`].
pub const DEFAULT_LINK_LIMIT: i64 = 1000;

/// CRUD for the `links` table — user/agent-authored directed relationships
/// between two objects (SOUL §5/§6.3). Every query is workspace-filtered
/// (SOUL §6.1/§18). Both endpoints are the core [`SourceRef`], stored split
/// across `(from_kind, from_id)` / `(to_kind, to_id)` via [`source_to_parts`].
#[derive(Clone, Debug)]
pub struct LinkRepo {
    pool: PgPool,
}

impl LinkRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create a directed `from → to` link in a workspace, authored by `author`.
    /// **Idempotent** on `(workspace_id, from, to, label)` (a NULL label folds to
    /// `''`): re-creating the same relationship refreshes its `note`/`updated_at`
    /// and returns the *existing* row (id preserved), never a duplicate. Rejects a
    /// self-link (`from == to`) with [`StoreError::Invalid`].
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        author: Author,
        from: &SourceRef,
        to: &SourceRef,
        label: Option<&str>,
        note: Option<&str>,
    ) -> Result<Link> {
        if from == to {
            return Err(StoreError::invalid("a link cannot point at itself"));
        }
        let id = LinkId::new().into_uuid();
        let (from_kind, from_id) = source_to_parts(from);
        let (to_kind, to_id) = source_to_parts(to);
        let (author_kind, author_id) = author_to_parts(author);
        let row: LinkRow = sqlx::query_as(&format!(
            "INSERT INTO links
                 (id, workspace_id, from_kind, from_id, to_kind, to_id, label, note,
                  author_kind, author_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
             ON CONFLICT (workspace_id, from_kind, from_id, to_kind, to_id, COALESCE(label, ''))
                 DO UPDATE SET note = EXCLUDED.note, updated_at = CURRENT_TIMESTAMP
             RETURNING {LINK_COLS}"
        ))
        .bind(id)
        .bind(workspace_id.into_uuid())
        .bind(from_kind)
        .bind(from_id)
        .bind(to_kind)
        .bind(to_id)
        .bind(label)
        .bind(note)
        .bind(author_kind)
        .bind(author_id)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Fetch a link, scoped to its workspace.
    pub async fn get(&self, workspace_id: WorkspaceId, id: LinkId) -> Result<Link> {
        let row: LinkRow = sqlx::query_as(&format!(
            "SELECT {LINK_COLS} FROM links
             WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// List a workspace's links, most-recently-touched first, bounded to `limit`
    /// rows (floored at 1, see [`DEFAULT_LINK_LIMIT`]).
    pub async fn list_by_workspace(
        &self,
        workspace_id: WorkspaceId,
        limit: i64,
    ) -> Result<Vec<Link>> {
        let rows: Vec<LinkRow> = sqlx::query_as(&format!(
            "SELECT {LINK_COLS} FROM links
             WHERE workspace_id = $1
             ORDER BY updated_at DESC, id ASC
             LIMIT $2"
        ))
        .bind(workspace_id.into_uuid())
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(Link::try_from).collect()
    }

    /// Every link touching `endpoint` in **either** direction (as `from` *or* as
    /// `to`) — "what is related to X" — workspace-scoped, most-recently-touched
    /// first, bounded to `limit` (floored at 1). Served by `links_from_idx` /
    /// `links_to_idx`.
    pub async fn list_for(
        &self,
        workspace_id: WorkspaceId,
        endpoint: &SourceRef,
        limit: i64,
    ) -> Result<Vec<Link>> {
        let (kind, id) = source_to_parts(endpoint);
        let rows: Vec<LinkRow> = sqlx::query_as(&format!(
            "SELECT {LINK_COLS} FROM links
             WHERE workspace_id = $1
               AND ((from_kind = $2 AND from_id = $3) OR (to_kind = $2 AND to_id = $3))
             ORDER BY updated_at DESC, id ASC
             LIMIT $4"
        ))
        .bind(workspace_id.into_uuid())
        .bind(kind)
        .bind(id)
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(Link::try_from).collect()
    }

    /// Delete a link, workspace-scoped. [`StoreError::NotFound`] if absent.
    pub async fn delete(&self, workspace_id: WorkspaceId, id: LinkId) -> Result<()> {
        let res = sqlx::query("DELETE FROM links WHERE id = $1 AND workspace_id = $2")
            .bind(id.into_uuid())
            .bind(workspace_id.into_uuid())
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
// Object labels — user/agent tags on stored files & directories (SOUL §9)
// ===========================================================================

/// The default cap on a labels listing so a large label set can't return an
/// unbounded payload (mirrors [`DEFAULT_LINK_LIMIT`]).
pub const DEFAULT_LABEL_LIMIT: i64 = 1000;

const OBJECT_LABEL_COLS: &str =
    "id, workspace_id, store, path, is_dir, label, author_kind, author_id, created_at";

/// CRUD for the `object_labels` table — free-text labels on a store's files and
/// directories (SOUL §9). A label is keyed by `(store, path)`, so it can tag a
/// **directory** (no object row) or an uncatalogued file. Every query is
/// workspace-filtered (SOUL §18).
#[derive(Clone, Debug)]
pub struct ObjectLabelRepo {
    pool: PgPool,
}

impl ObjectLabelRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Apply `label` to `path` in `store`, authored by `author`. **Idempotent** on
    /// `(workspace_id, store, path, label)`: re-applying the same label refreshes
    /// `is_dir`/`created_at` and returns the *existing* row (id preserved), never a
    /// duplicate. Rejects a blank `label` or `path` with [`StoreError::Invalid`].
    pub async fn add(
        &self,
        workspace_id: WorkspaceId,
        author: Author,
        store: &str,
        path: &str,
        is_dir: bool,
        label: &str,
    ) -> Result<ObjectLabel> {
        let path = path.trim();
        let label = label.trim();
        if path.is_empty() {
            return Err(StoreError::invalid("a label's path must not be empty"));
        }
        if label.is_empty() {
            return Err(StoreError::invalid("a label must not be empty"));
        }
        let id = ObjectLabelId::new().into_uuid();
        let (author_kind, author_id) = author_to_parts(author);
        let row: ObjectLabelRow = sqlx::query_as(&format!(
            "INSERT INTO object_labels
                 (id, workspace_id, store, path, is_dir, label, author_kind, author_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (workspace_id, store, path, label)
                 DO UPDATE SET is_dir = EXCLUDED.is_dir
             RETURNING {OBJECT_LABEL_COLS}"
        ))
        .bind(id)
        .bind(workspace_id.into_uuid())
        .bind(store)
        .bind(path)
        .bind(is_dir)
        .bind(label)
        .bind(author_kind)
        .bind(author_id)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Every label on one `(store, path)`, oldest-first, workspace-scoped.
    pub async fn list_for(
        &self,
        workspace_id: WorkspaceId,
        store: &str,
        path: &str,
    ) -> Result<Vec<ObjectLabel>> {
        let rows: Vec<ObjectLabelRow> = sqlx::query_as(&format!(
            "SELECT {OBJECT_LABEL_COLS} FROM object_labels
             WHERE workspace_id = $1 AND store = $2 AND path = $3
             ORDER BY created_at ASC, id ASC"
        ))
        .bind(workspace_id.into_uuid())
        .bind(store)
        .bind(path)
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(ObjectLabel::try_from).collect()
    }

    /// A store's labels, optionally restricted to paths under `prefix` (a *literal*
    /// prefix — empty = the whole store), most-recent first, bounded to `limit`
    /// (floored at 1). The Files panel badges its tree from this.
    pub async fn list_by_store(
        &self,
        workspace_id: WorkspaceId,
        store: &str,
        prefix: &str,
        limit: i64,
    ) -> Result<Vec<ObjectLabel>> {
        let rows: Vec<ObjectLabelRow> = sqlx::query_as(&format!(
            "SELECT {OBJECT_LABEL_COLS} FROM object_labels
             WHERE workspace_id = $1 AND store = $2
               AND ($3 = '' OR starts_with(path, $3))
             ORDER BY created_at DESC, id ASC
             LIMIT $4"
        ))
        .bind(workspace_id.into_uuid())
        .bind(store)
        .bind(prefix)
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(ObjectLabel::try_from).collect()
    }

    /// Every path carrying `label` across all stores in a workspace, most-recent
    /// first, bounded to `limit` (floored at 1) — the label filter.
    pub async fn list_by_label(
        &self,
        workspace_id: WorkspaceId,
        label: &str,
        limit: i64,
    ) -> Result<Vec<ObjectLabel>> {
        let rows: Vec<ObjectLabelRow> = sqlx::query_as(&format!(
            "SELECT {OBJECT_LABEL_COLS} FROM object_labels
             WHERE workspace_id = $1 AND label = $2
             ORDER BY created_at DESC, id ASC
             LIMIT $3"
        ))
        .bind(workspace_id.into_uuid())
        .bind(label)
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(ObjectLabel::try_from).collect()
    }

    /// Every label on any of the given `(store, path)` pairs in **one** query,
    /// workspace-scoped (SOUL §9/§18) — the batched form of
    /// [`list_for`](Self::list_for), so a page of object summaries can carry its
    /// label sets without an N+1. A pair with no labels is simply absent from
    /// the result; empty `pairs` → empty (no query). Oldest-first per the
    /// `list_for` order (then id for a stable tie-break).
    #[cfg(not(feature = "sqlite"))]
    pub async fn list_for_paths(
        &self,
        workspace_id: WorkspaceId,
        pairs: &[(String, String)],
    ) -> Result<Vec<ObjectLabel>> {
        if pairs.is_empty() {
            return Ok(Vec::new());
        }
        let (stores, paths): (Vec<String>, Vec<String>) = pairs.iter().cloned().unzip();
        let rows: Vec<ObjectLabelRow> = sqlx::query_as(&format!(
            "SELECT {OBJECT_LABEL_COLS} FROM object_labels
             WHERE workspace_id = $1
               AND (store, path) IN
                   (SELECT s, p FROM unnest($2::text[], $3::text[]) AS t(s, p))
             ORDER BY created_at ASC, id ASC"
        ))
        .bind(workspace_id.into_uuid())
        .bind(&stores)
        .bind(&paths)
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(ObjectLabel::try_from).collect()
    }

    #[cfg(feature = "sqlite")]
    pub async fn list_for_paths(
        &self,
        workspace_id: WorkspaceId,
        pairs: &[(String, String)],
    ) -> Result<Vec<ObjectLabel>> {
        if pairs.is_empty() {
            return Ok(Vec::new());
        }
        let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(format!(
            "SELECT {OBJECT_LABEL_COLS} FROM object_labels WHERE workspace_id = "
        ));
        query.push_bind(workspace_id.into_uuid()).push(" AND (");
        {
            let mut clauses = query.separated(" OR ");
            for (store, path) in pairs {
                clauses
                    .push("(store = ")
                    .push_bind(store)
                    .push(" AND path = ")
                    .push_bind(path)
                    .push_unseparated(")");
            }
        }
        query.push(") ORDER BY created_at ASC, id ASC");
        let rows: Vec<ObjectLabelRow> = query
            .build_query_as()
            .fetch_all(&self.pool)
            .await
            .map_err(map)?;
        rows.into_iter().map(ObjectLabel::try_from).collect()
    }

    /// Delete a label by id, workspace-scoped. [`StoreError::NotFound`] if absent.
    pub async fn delete(&self, workspace_id: WorkspaceId, id: ObjectLabelId) -> Result<()> {
        let res = sqlx::query("DELETE FROM object_labels WHERE id = $1 AND workspace_id = $2")
            .bind(id.into_uuid())
            .bind(workspace_id.into_uuid())
            .execute(&self.pool)
            .await
            .map_err(map)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// Purge every label on one `(store, path)` — used to keep labels in sync when
    /// a file's bytes are deleted so a label can't outlive its file. Idempotent:
    /// returns how many rows were removed (0 when nothing was labelled).
    pub async fn delete_for_path(
        &self,
        workspace_id: WorkspaceId,
        store: &str,
        path: &str,
    ) -> Result<u64> {
        let res = sqlx::query(
            "DELETE FROM object_labels
             WHERE workspace_id = $1 AND store = $2 AND path = $3",
        )
        .bind(workspace_id.into_uuid())
        .bind(store)
        .bind(path)
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(res.rows_affected())
    }
}

// ===========================================================================
// emerged UIs
// ===========================================================================

const UI_DEF_COLS: &str = "id, workspace_id, author_kind, author_id, name, title, description, \
     spec_version, version, definition, created_at, updated_at";

/// The mutable fields of an emerged UI, shared by create + update (keeps both
/// repository calls under the argument-count lint, like the other `New*` inputs).
#[derive(Clone, Debug)]
pub struct UiDefinitionInput {
    /// Optional slug, unique-when-set per workspace.
    pub name: Option<String>,
    /// Human title.
    pub title: String,
    /// Optional description.
    pub description: Option<String>,
    /// The component tree.
    pub definition: UiSpec,
}

/// CRUD for the `ui_definitions` table — AI-authored emerged UIs (declarative
/// component trees). Every query is workspace-filtered (SOUL §6.1/§18); edits
/// use an optimistic `version` counter so concurrent patches don't clobber.
#[derive(Clone, Debug)]
pub struct UiDefinitionRepo {
    pool: PgPool,
}

impl UiDefinitionRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create an emerged UI, authored by `author`. `input.name` is an optional
    /// slug, unique-when-set per workspace ([`StoreError::Conflict`] on
    /// collision). Returns the stored definition at `version = 1`.
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        author: Author,
        spec_version: u32,
        input: &UiDefinitionInput,
    ) -> Result<UiDefinition> {
        let id = UiDefinitionId::new().into_uuid();
        let (author_kind, author_id) = author_to_parts(author);
        let row: UiDefinitionRow = sqlx::query_as(&format!(
            "INSERT INTO ui_definitions
                 (id, workspace_id, author_kind, author_id, name, title, description,
                  spec_version, definition)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             RETURNING {UI_DEF_COLS}"
        ))
        .bind(id)
        .bind(workspace_id.into_uuid())
        .bind(author_kind)
        .bind(author_id)
        .bind(input.name.as_deref())
        .bind(&input.title)
        .bind(input.description.as_deref())
        .bind(i32::try_from(spec_version).unwrap_or(i32::MAX))
        .bind(Json(input.definition.clone()))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Fetch an emerged UI by id, scoped to its workspace.
    pub async fn get(&self, workspace_id: WorkspaceId, id: UiDefinitionId) -> Result<UiDefinition> {
        let row: UiDefinitionRow = sqlx::query_as(&format!(
            "SELECT {UI_DEF_COLS} FROM ui_definitions
             WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Fetch an emerged UI by its (workspace-unique) name.
    pub async fn get_by_name(&self, workspace_id: WorkspaceId, name: &str) -> Result<UiDefinition> {
        let row: UiDefinitionRow = sqlx::query_as(&format!(
            "SELECT {UI_DEF_COLS} FROM ui_definitions
             WHERE workspace_id = $1 AND name = $2"
        ))
        .bind(workspace_id.into_uuid())
        .bind(name)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// List a workspace's emerged UIs, most-recently-edited first. The
    /// `definition` JSONB is returned too; callers wanting a compact list should
    /// project the fields they need.
    pub async fn list_by_workspace(&self, workspace_id: WorkspaceId) -> Result<Vec<UiDefinition>> {
        let rows: Vec<UiDefinitionRow> = sqlx::query_as(&format!(
            "SELECT {UI_DEF_COLS} FROM ui_definitions
             WHERE workspace_id = $1
             ORDER BY updated_at DESC, id ASC"
        ))
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(UiDefinition::try_from).collect()
    }

    /// Optimistically update a UI's metadata + definition, bumping `version`.
    /// The update only applies when the stored `version` equals `expected`,
    /// guarding against a concurrent edit ([`StoreError::Conflict`] otherwise).
    /// The author and `spec_version` are immutable here.
    pub async fn update_definition(
        &self,
        workspace_id: WorkspaceId,
        id: UiDefinitionId,
        expected: i64,
        input: &UiDefinitionInput,
    ) -> Result<UiDefinition> {
        let row: Option<UiDefinitionRow> = sqlx::query_as(&format!(
            "UPDATE ui_definitions
                SET name = $4, title = $5, description = $6, definition = $7,
                    version = version + 1, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2 AND version = $3
             RETURNING {UI_DEF_COLS}"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(expected)
        .bind(input.name.as_deref())
        .bind(&input.title)
        .bind(input.description.as_deref())
        .bind(Json(input.definition.clone()))
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        match row {
            Some(r) => r.try_into(),
            None => Err(StoreError::Conflict(format!(
                "ui {id}: stale version {expected} (or not found in workspace)"
            ))),
        }
    }

    /// Delete an emerged UI, workspace-scoped. [`StoreError::NotFound`] if absent.
    pub async fn delete(&self, workspace_id: WorkspaceId, id: UiDefinitionId) -> Result<()> {
        let res = sqlx::query("DELETE FROM ui_definitions WHERE id = $1 AND workspace_id = $2")
            .bind(id.into_uuid())
            .bind(workspace_id.into_uuid())
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
// Pending questions (ask_user, SOUL §7/§12)
// ===========================================================================

/// The full set of `pending_questions` columns, in row order.
const PENDING_QUESTION_COLS: &str =
    "id, workspace_id, conversation_id, questions, created_at, resolved_at, answers";

/// CRUD for the `pending_questions` table — unanswered `ask_user` question forms.
/// Every query is workspace-filtered (SOUL §18).
#[derive(Clone, Debug)]
pub struct PendingQuestionRepo {
    pool: PgPool,
}

impl PendingQuestionRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Persist a new pending question for a conversation.
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        conversation_id: ConversationId,
        questions: &[Question],
    ) -> Result<PendingQuestion> {
        let id = PendingQuestionId::new().into_uuid();
        let row: PendingQuestionRow = sqlx::query_as(&format!(
            "INSERT INTO pending_questions (id, workspace_id, conversation_id, questions)
             VALUES ($1, $2, $3, $4)
             RETURNING {PENDING_QUESTION_COLS}"
        ))
        .bind(id)
        .bind(workspace_id.into_uuid())
        .bind(conversation_id.into_uuid())
        .bind(Json(questions))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// The most-recent **unresolved** question for a conversation, if any — what the
    /// client fetches on load to re-render the form after a reload/reconnect.
    pub async fn get_unresolved(
        &self,
        workspace_id: WorkspaceId,
        conversation_id: ConversationId,
    ) -> Result<Option<PendingQuestion>> {
        let row: Option<PendingQuestionRow> = sqlx::query_as(&format!(
            "SELECT {PENDING_QUESTION_COLS} FROM pending_questions
             WHERE workspace_id = $1 AND conversation_id = $2 AND resolved_at IS NULL
             ORDER BY created_at DESC
             LIMIT 1"
        ))
        .bind(workspace_id.into_uuid())
        .bind(conversation_id.into_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.map(Into::into))
    }

    /// Resolve **every** unresolved question for a conversation (idempotent). Called
    /// when the user answers or otherwise moves the thread on, so at most one stays
    /// pending. `answers` is the structured form reply, stamped onto the row(s) this
    /// call closes so the Q&A exchange stays durable; `None` (the user typed past
    /// the form / a fresh `ask_user` superseded it) leaves the column NULL — the
    /// question was never answered. Returns how many rows it closed.
    pub async fn resolve_for_conversation(
        &self,
        workspace_id: WorkspaceId,
        conversation_id: ConversationId,
        answers: Option<&[Answer]>,
    ) -> Result<u64> {
        let res = sqlx::query(
            "UPDATE pending_questions SET resolved_at = CURRENT_TIMESTAMP, answers = $3
             WHERE workspace_id = $1 AND conversation_id = $2 AND resolved_at IS NULL",
        )
        .bind(workspace_id.into_uuid())
        .bind(conversation_id.into_uuid())
        .bind(answers.map(Json))
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(res.rows_affected())
    }

    /// Every question ever asked in a conversation — resolved and pending, oldest
    /// first. The chat client fetches this when replaying a transcript, so an
    /// `ask_user` exchange re-renders with the answers the user actually gave
    /// (correlated to the tool call via the `pending_question_id` in its result).
    pub async fn list_for_conversation(
        &self,
        workspace_id: WorkspaceId,
        conversation_id: ConversationId,
    ) -> Result<Vec<PendingQuestion>> {
        let rows: Vec<PendingQuestionRow> = sqlx::query_as(&format!(
            "SELECT {PENDING_QUESTION_COLS} FROM pending_questions
             WHERE workspace_id = $1 AND conversation_id = $2
             ORDER BY created_at ASC"
        ))
        .bind(workspace_id.into_uuid())
        .bind(conversation_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Into::into).collect())
    }
}

/// The full set of `pending_approvals` columns, in row order.
const PENDING_APPROVAL_COLS: &str =
    "id, workspace_id, conversation_id, tool, arguments, reason, created_at, resolved_at, decision";

/// CRUD for the `pending_approvals` table — guard-deferred tool calls awaiting the
/// user's Approve/Reject (SOUL §7/§12/§19). Every query is workspace-filtered.
#[derive(Clone, Debug)]
pub struct PendingApprovalRepo {
    pool: PgPool,
}

impl PendingApprovalRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Record a new deferred approval for a conversation (unresolved).
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        conversation_id: ConversationId,
        tool: &str,
        arguments: &serde_json::Value,
        reason: &str,
    ) -> Result<PendingApproval> {
        let id = PendingApprovalId::new().into_uuid();
        let row: PendingApprovalRow = sqlx::query_as(&format!(
            "INSERT INTO pending_approvals
                 (id, workspace_id, conversation_id, tool, arguments, reason)
             VALUES ($1, $2, $3, $4, $5, $6)
             RETURNING {PENDING_APPROVAL_COLS}"
        ))
        .bind(id)
        .bind(workspace_id.into_uuid())
        .bind(conversation_id.into_uuid())
        .bind(tool)
        .bind(Json(arguments))
        .bind(reason)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// The most-recent **unresolved** approval for a conversation, if any — what the
    /// client fetches on load to re-render the Approve/Reject prompt after a reload /
    /// reconnect / restart.
    pub async fn get_unresolved(
        &self,
        workspace_id: WorkspaceId,
        conversation_id: ConversationId,
    ) -> Result<Option<PendingApproval>> {
        let row: Option<PendingApprovalRow> = sqlx::query_as(&format!(
            "SELECT {PENDING_APPROVAL_COLS} FROM pending_approvals
             WHERE workspace_id = $1 AND conversation_id = $2 AND resolved_at IS NULL
             ORDER BY created_at DESC
             LIMIT 1"
        ))
        .bind(workspace_id.into_uuid())
        .bind(conversation_id.into_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.map(Into::into))
    }

    /// Record the user's decision on a pending approval by id (idempotent: a no-op
    /// if it was already resolved/superseded). Returns the resolved row (so the
    /// caller knows the tool + args to resume), or `None` if there was nothing
    /// unresolved under that id.
    pub async fn resolve(
        &self,
        workspace_id: WorkspaceId,
        id: PendingApprovalId,
        decision: ApprovalDecision,
    ) -> Result<Option<PendingApproval>> {
        let decision_text = match decision {
            ApprovalDecision::Approved => "approved",
            ApprovalDecision::Rejected => "rejected",
        };
        let row: Option<PendingApprovalRow> = sqlx::query_as(&format!(
            "UPDATE pending_approvals SET resolved_at = CURRENT_TIMESTAMP, decision = $3
             WHERE workspace_id = $1 AND id = $2 AND resolved_at IS NULL
             RETURNING {PENDING_APPROVAL_COLS}"
        ))
        .bind(workspace_id.into_uuid())
        .bind(id.into_uuid())
        .bind(decision_text)
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.map(Into::into))
    }

    /// Supersede **every** unresolved approval for a conversation (idempotent),
    /// leaving `decision` NULL — the deferred tool is abandoned (the user moved the
    /// thread on without deciding). Returns how many rows it closed.
    pub async fn resolve_for_conversation(
        &self,
        workspace_id: WorkspaceId,
        conversation_id: ConversationId,
    ) -> Result<u64> {
        let res = sqlx::query(
            "UPDATE pending_approvals SET resolved_at = CURRENT_TIMESTAMP
             WHERE workspace_id = $1 AND conversation_id = $2 AND resolved_at IS NULL",
        )
        .bind(workspace_id.into_uuid())
        .bind(conversation_id.into_uuid())
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(res.rows_affected())
    }

    /// The resume path: consume a **resolved** decision matching a re-attempted call
    /// (`(conversation, tool, arguments)`), deleting the row and returning its
    /// ruling. The guard calls this when the agent re-issues the approved/rejected
    /// call so the decision applies exactly once. `None` when nothing matches.
    pub async fn take_resolved(
        &self,
        workspace_id: WorkspaceId,
        conversation_id: ConversationId,
        tool: &str,
        arguments: &serde_json::Value,
    ) -> Result<Option<ApprovalDecision>> {
        let decision: Option<String> = sqlx::query_scalar(
            "DELETE FROM pending_approvals WHERE id = (
                 SELECT id FROM pending_approvals
                 WHERE workspace_id = $1 AND conversation_id = $2 AND tool = $3
                   AND arguments = $4 AND resolved_at IS NOT NULL AND decision IS NOT NULL
                 ORDER BY resolved_at DESC
                 LIMIT 1
             )
             RETURNING decision",
        )
        .bind(workspace_id.into_uuid())
        .bind(conversation_id.into_uuid())
        .bind(tool)
        .bind(Json(arguments))
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        Ok(decision.and_then(|d| match d.as_str() {
            "approved" => Some(ApprovalDecision::Approved),
            "rejected" => Some(ApprovalDecision::Rejected),
            _ => None,
        }))
    }
}

// ===========================================================================
// Connections
// ===========================================================================

/// The full set of `connections` columns, in row order (for `SELECT`/`RETURNING`).
const CONNECTION_COLS: &str =
    "id, workspace_id, kind, name, credential_ref, config, sync_token, created_at, updated_at";

/// CRUD for the `connections` table — provider connections (calendar / storage /
/// channel, SOUL §6.1/§8). Per-provider settings (local dir path, CalDAV base
/// URL, …) ride in the `config` JSON blob; the incremental-sync position rides
/// in `sync_token` (the core `Connection::cursor`).
#[derive(Clone, Debug)]
pub struct ConnectionRepo {
    pool: PgPool,
}

impl ConnectionRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create a connection in a workspace. `config` is the per-provider settings
    /// blob (defaults to `{}` if `None`); `credential_ref` points into the
    /// secret store (never plaintext, SOUL §13).
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        kind: ConnectionKind,
        name: &str,
        credential_ref: Option<&str>,
        config: Option<serde_json::Value>,
    ) -> Result<Connection> {
        let id = ConnectionId::new().into_uuid();
        let kind_text = connection_kind_to_text(kind)?;
        let config = config.unwrap_or_else(|| serde_json::json!({}));
        let row: ConnectionRow = sqlx::query_as(&format!(
            "INSERT INTO connections (id, workspace_id, kind, name, credential_ref, config)
             VALUES ($1, $2, $3, $4, $5, $6)
             RETURNING {CONNECTION_COLS}"
        ))
        .bind(id)
        .bind(workspace_id.into_uuid())
        .bind(kind_text)
        .bind(name)
        .bind(credential_ref)
        .bind(Json(config))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Get-or-create a connection by `(workspace_id, kind, name)`. Idempotent and
    /// **race-free** at the DB level (the `connections_workspace_kind_name_uq`
    /// constraint): concurrent callers converge on one row instead of the
    /// application-level find-or-create's TOCTOU window. On conflict the existing
    /// row's id is preserved and `credential_ref` is set only if the row had none
    /// (`COALESCE`), so a re-ensure never clobbers a stored secret. `config` is
    /// **refreshed when provided** (`Some`) and **preserved when omitted** (`None`,
    /// also `COALESCE`): a caller that re-ensures with new settings (e.g. the email
    /// driver moving a Maildir's `root`) sees them take effect, while a caller that
    /// only ensures existence (e.g. the storage catalogue passing `None`) never
    /// wipes an existing blob. This is the safe get-or-create the storage catalogue
    /// uses before cataloguing an upload (SOUL §9/§3.4).
    pub async fn ensure(
        &self,
        workspace_id: WorkspaceId,
        kind: ConnectionKind,
        name: &str,
        credential_ref: Option<&str>,
        config: Option<serde_json::Value>,
    ) -> Result<Connection> {
        let id = ConnectionId::new().into_uuid();
        let kind_text = connection_kind_to_text(kind)?;
        // `$6` is NULL when no config is supplied: the INSERT defaults it to `{}`,
        // and the conflict UPDATE keeps the existing config (COALESCE). When a
        // config IS supplied it both inserts and refreshes the stored config.
        let config = config.map(Json);
        #[cfg(not(feature = "sqlite"))]
        let statement = format!(
            "INSERT INTO connections (id, workspace_id, kind, name, credential_ref, config)
             VALUES ($1, $2, $3, $4, $5, COALESCE($6::jsonb, '{{}}'::jsonb))
             ON CONFLICT (workspace_id, kind, name) DO UPDATE SET
                 credential_ref = COALESCE(EXCLUDED.credential_ref, connections.credential_ref),
                 config = COALESCE($6::jsonb, connections.config),
                 updated_at = CURRENT_TIMESTAMP
             RETURNING {CONNECTION_COLS}"
        );
        #[cfg(feature = "sqlite")]
        let statement = format!(
            "INSERT INTO connections (id, workspace_id, kind, name, credential_ref, config)
             VALUES ($1, $2, $3, $4, $5, COALESCE($6, '{{}}'))
             ON CONFLICT (workspace_id, kind, name) DO UPDATE SET
                 credential_ref = COALESCE(EXCLUDED.credential_ref, connections.credential_ref),
                 config = COALESCE($6, connections.config),
                 updated_at = CURRENT_TIMESTAMP
             RETURNING {CONNECTION_COLS}"
        );
        let row: ConnectionRow = sqlx::query_as(&statement)
            .bind(id)
            .bind(workspace_id.into_uuid())
            .bind(kind_text)
            .bind(name)
            .bind(credential_ref)
            .bind(config)
            .fetch_one(&self.pool)
            .await
            .map_err(map)?;
        row.try_into()
    }

    /// Reconcile a config-defined connection by `(workspace_id, kind, name)`.
    /// Unlike [`Self::ensure`], the credential is replaced **exactly**, including
    /// clearing it when `credential_ref` is `None`. Immutable deployment config
    /// is authoritative for a same-named connection, while the row identity is
    /// preserved so automation triggers and migration ledgers keep their foreign
    /// keys across restarts.
    pub async fn reconcile_configured(
        &self,
        workspace_id: WorkspaceId,
        kind: ConnectionKind,
        name: &str,
        credential_ref: Option<&str>,
        config: serde_json::Value,
    ) -> Result<Connection> {
        let id = ConnectionId::new().into_uuid();
        let kind_text = connection_kind_to_text(kind)?;
        #[cfg(not(feature = "sqlite"))]
        let statement = format!(
            "INSERT INTO connections (id, workspace_id, kind, name, credential_ref, config)
             VALUES ($1, $2, $3, $4, $5, $6::jsonb)
             ON CONFLICT (workspace_id, kind, name) DO UPDATE SET
                 credential_ref = EXCLUDED.credential_ref,
                 config = EXCLUDED.config,
                 updated_at = CURRENT_TIMESTAMP
             RETURNING {CONNECTION_COLS}"
        );
        #[cfg(feature = "sqlite")]
        let statement = format!(
            "INSERT INTO connections (id, workspace_id, kind, name, credential_ref, config)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (workspace_id, kind, name) DO UPDATE SET
                 credential_ref = EXCLUDED.credential_ref,
                 config = EXCLUDED.config,
                 updated_at = CURRENT_TIMESTAMP
             RETURNING {CONNECTION_COLS}"
        );
        let row: ConnectionRow = sqlx::query_as(&statement)
            .bind(id)
            .bind(workspace_id.into_uuid())
            .bind(kind_text)
            .bind(name)
            .bind(credential_ref)
            .bind(Json(config))
            .fetch_one(&self.pool)
            .await
            .map_err(map)?;
        row.try_into()
    }

    /// Fetch a connection, scoped to its workspace.
    pub async fn get(&self, workspace_id: WorkspaceId, id: ConnectionId) -> Result<Connection> {
        let row: ConnectionRow = sqlx::query_as(&format!(
            "SELECT {CONNECTION_COLS} FROM connections
             WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Fetch the full row (including store-only `config`/timestamps), scoped to
    /// its workspace. Use this when the `config` blob or timestamps are needed.
    pub async fn get_row(
        &self,
        workspace_id: WorkspaceId,
        id: ConnectionId,
    ) -> Result<ConnectionRow> {
        sqlx::query_as(&format!(
            "SELECT {CONNECTION_COLS} FROM connections
             WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)
    }

    /// List a workspace's connections, newest first.
    pub async fn list_by_workspace(&self, workspace_id: WorkspaceId) -> Result<Vec<Connection>> {
        let rows: Vec<ConnectionRow> = sqlx::query_as(&format!(
            "SELECT {CONNECTION_COLS} FROM connections
             WHERE workspace_id = $1
             ORDER BY created_at DESC"
        ))
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(Connection::try_from).collect()
    }

    /// Replace a connection's `name` + per-provider `config` blob, workspace-scoped
    /// (SOUL §28 — editing an existing source from the workbench). Leaves
    /// `credential_ref` and `sync_token` untouched; a caller that lets users omit
    /// unchanged secrets must merge them from the existing blob **before** calling
    /// (this is a full config replace). Returns the updated connection.
    pub async fn update_named_config(
        &self,
        workspace_id: WorkspaceId,
        id: ConnectionId,
        name: &str,
        config: serde_json::Value,
    ) -> Result<Connection> {
        let row: ConnectionRow = sqlx::query_as(&format!(
            "UPDATE connections SET name = $3, config = $4, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2
             RETURNING {CONNECTION_COLS}"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(name)
        .bind(Json(config))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Update the incremental-sync cursor (`sync_token`), workspace-scoped. Pass
    /// `None` to clear it (e.g. to force a full re-sync). Returns the updated
    /// connection.
    pub async fn update_cursor(
        &self,
        workspace_id: WorkspaceId,
        id: ConnectionId,
        cursor: Option<&Cursor>,
    ) -> Result<Connection> {
        let token = cursor.map(|c| c.0.as_str());
        let row: ConnectionRow = sqlx::query_as(&format!(
            "UPDATE connections SET sync_token = $3, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2
             RETURNING {CONNECTION_COLS}"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(token)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Set (or clear) a connection's Google push **watch state** under the
    /// `"watch"` key of its `config` JSONB, workspace-scoped (SOUL §8/§16 M7 push
    /// half). This rides the existing `config` blob rather than a new column: the
    /// value is provider-managed runtime state (the live `events.watch` channel id /
    /// resource id / expiry) that the calendar factory ignores, kept beside the
    /// user-set provider keys. `Some(v)` writes/replaces `config.watch = v`;
    /// `None` removes the key (a stopped/absent watch). Atomic per row (a single
    /// `jsonb_set` / `-` update), so it never races the OAuth callback's
    /// `credential_ref` seal (which touches only the secret store). Returns the
    /// updated connection.
    #[cfg(not(feature = "sqlite"))]
    pub async fn set_watch_state(
        &self,
        workspace_id: WorkspaceId,
        id: ConnectionId,
        watch: Option<serde_json::Value>,
    ) -> Result<Connection> {
        // `jsonb_set(config, '{watch}', $3, true)` inserts-or-replaces the key;
        // `config - 'watch'` removes it. Branch on presence so a clear truly drops
        // the key (rather than storing `null`, which the scan would misread as live).
        let row: ConnectionRow = match watch {
            Some(v) => sqlx::query_as(&format!(
                "UPDATE connections
                    SET config = jsonb_set(COALESCE(config, '{{}}'::jsonb), '{{watch}}', $3::jsonb, true),
                        updated_at = CURRENT_TIMESTAMP
                  WHERE id = $1 AND workspace_id = $2
                  RETURNING {CONNECTION_COLS}"
            ))
            .bind(id.into_uuid())
            .bind(workspace_id.into_uuid())
            .bind(Json(v))
            .fetch_one(&self.pool)
            .await
            .map_err(map)?,
            None => sqlx::query_as(&format!(
                "UPDATE connections
                    SET config = COALESCE(config, '{{}}'::jsonb) - 'watch',
                        updated_at = CURRENT_TIMESTAMP
                  WHERE id = $1 AND workspace_id = $2
                  RETURNING {CONNECTION_COLS}"
            ))
            .bind(id.into_uuid())
            .bind(workspace_id.into_uuid())
            .fetch_one(&self.pool)
            .await
            .map_err(map)?,
        };
        row.try_into()
    }

    /// SQLite JSON1 implementation of [`Self::set_watch_state`]. Keeping this as
    /// a compile-time specialization avoids runtime dialect branches.
    #[cfg(feature = "sqlite")]
    pub async fn set_watch_state(
        &self,
        workspace_id: WorkspaceId,
        id: ConnectionId,
        watch: Option<serde_json::Value>,
    ) -> Result<Connection> {
        let row: ConnectionRow = match watch {
            Some(value) => sqlx::query_as(&format!(
                "UPDATE connections
                    SET config = json_set(COALESCE(config, '{{}}'), '$.watch', json($3)),
                        updated_at = CURRENT_TIMESTAMP
                  WHERE id = $1 AND workspace_id = $2
                  RETURNING {CONNECTION_COLS}"
            ))
            .bind(id.into_uuid())
            .bind(workspace_id.into_uuid())
            .bind(Json(value))
            .fetch_one(&self.pool)
            .await
            .map_err(map)?,
            None => sqlx::query_as(&format!(
                "UPDATE connections
                    SET config = json_remove(COALESCE(config, '{{}}'), '$.watch'),
                        updated_at = CURRENT_TIMESTAMP
                  WHERE id = $1 AND workspace_id = $2
                  RETURNING {CONNECTION_COLS}"
            ))
            .bind(id.into_uuid())
            .bind(workspace_id.into_uuid())
            .fetch_one(&self.pool)
            .await
            .map_err(map)?,
        };
        row.try_into()
    }

    /// Point a connection at a sealed credential (its `credential_ref`), or clear it
    /// (`None`), workspace-scoped. This is the minimal set-credential mutation the
    /// Google flows need: the OAuth callback uses it to **re-authorize a
    /// `credential_ref`-less connection in place** (sealing a fresh secret and
    /// pointing the existing row at it, rather than creating a duplicate), and
    /// opportunistic Gmail resealing uses it to swap a legacy plaintext connection
    /// onto the encrypted store (SOUL §13/§28). Touches **only** the `credential_ref`
    /// column, never `config`, so it never races the `config`-only writers
    /// (`set_watch_state` / `scrub_config_keys`) — mirroring `set_watch_state`'s
    /// column-scoped discipline. Returns the updated connection.
    pub async fn set_credential_ref(
        &self,
        workspace_id: WorkspaceId,
        id: ConnectionId,
        credential_ref: Option<&str>,
    ) -> Result<Connection> {
        let row: ConnectionRow = sqlx::query_as(&format!(
            "UPDATE connections SET credential_ref = $3, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2
             RETURNING {CONNECTION_COLS}"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(credential_ref)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Remove the named top-level keys from a connection's `config` JSONB,
    /// workspace-scoped — the scoped, additive mutation opportunistic Gmail resealing
    /// uses to strip the now-redundant plaintext OAuth credentials
    /// (`client_id`/`client_secret`/`refresh_token`) once they've been sealed
    /// (SOUL §13/§28). Deliberately **targeted** (a single `config - '{keys}'::text[]`
    /// delete) rather than a whole-blob overwrite: it removes only the named keys and
    /// leaves every other key intact, so it can't clobber a concurrent additive
    /// `config` writer (the `set_watch_state` `jsonb_set` path) the way an
    /// `update_config(id, whole_json)` would — it mirrors that additive discipline.
    /// A key that isn't present is a no-op; passing no keys leaves `config`
    /// unchanged. Returns the updated connection.
    #[cfg(not(feature = "sqlite"))]
    pub async fn scrub_config_keys(
        &self,
        workspace_id: WorkspaceId,
        id: ConnectionId,
        keys: &[&str],
    ) -> Result<Connection> {
        // `jsonb - text[]` deletes every matching top-level key in one shot (leaving
        // the rest of the blob untouched), so this is a single atomic row update.
        let keys: Vec<String> = keys.iter().map(|k| (*k).to_string()).collect();
        let row: ConnectionRow = sqlx::query_as(&format!(
            "UPDATE connections
                SET config = COALESCE(config, '{{}}'::jsonb) - $3::text[],
                    updated_at = CURRENT_TIMESTAMP
              WHERE id = $1 AND workspace_id = $2
              RETURNING {CONNECTION_COLS}"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(&keys)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    #[cfg(feature = "sqlite")]
    pub async fn scrub_config_keys(
        &self,
        workspace_id: WorkspaceId,
        id: ConnectionId,
        keys: &[&str],
    ) -> Result<Connection> {
        let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
            "UPDATE connections SET config = json_remove(COALESCE(config, '{}')",
        );
        for key in keys {
            // Provider config keys are identifiers, but quoting the JSON path is
            // still required for punctuation and makes this safe for arbitrary
            // future keys. JSON encoding supplies the correct escaping.
            let quoted = serde_json::to_string(key).map_err(StoreError::invalid)?;
            query.push(", ").push_bind(format!("$.{quoted}"));
        }
        query
            .push("), updated_at = CURRENT_TIMESTAMP WHERE id = ")
            .push_bind(id.into_uuid())
            .push(" AND workspace_id = ")
            .push_bind(workspace_id.into_uuid())
            .push(format!(" RETURNING {CONNECTION_COLS}"));
        let row: ConnectionRow = query
            .build_query_as()
            .fetch_one(&self.pool)
            .await
            .map_err(map)?;
        row.try_into()
    }

    /// Delete a connection (cascades to its calendars and events),
    /// workspace-scoped.
    pub async fn delete(&self, workspace_id: WorkspaceId, id: ConnectionId) -> Result<()> {
        let res = sqlx::query("DELETE FROM connections WHERE id = $1 AND workspace_id = $2")
            .bind(id.into_uuid())
            .bind(workspace_id.into_uuid())
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
// Calendars
// ===========================================================================

/// The full set of `calendars` columns, in row order.
const CALENDAR_COLS: &str =
    "id, workspace_id, connection_id, external_id, name, read_only, created_at";

/// CRUD for the `calendars` table (SOUL §6.1/§8). Calendars belong to a
/// connection; `upsert` is keyed on `(connection_id, external_id)` so
/// re-listing a provider's calendars is idempotent.
#[derive(Clone, Debug)]
pub struct CalendarRepo {
    pool: PgPool,
}

impl CalendarRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Upsert a calendar by `(connection_id, external_id)`. On conflict the
    /// `name` and `read_only` flag are refreshed; the id is preserved. Idempotent
    /// (SOUL §3.4): re-listing the provider's calendars never duplicates.
    pub async fn upsert(
        &self,
        workspace_id: WorkspaceId,
        connection_id: ConnectionId,
        external_id: &str,
        name: &str,
        read_only: bool,
    ) -> Result<Calendar> {
        let id = CalendarId::new().into_uuid();
        let row: CalendarRow = sqlx::query_as(&format!(
            "INSERT INTO calendars (id, workspace_id, connection_id, external_id, name, read_only)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (connection_id, external_id)
             DO UPDATE SET name = EXCLUDED.name, read_only = EXCLUDED.read_only
             RETURNING {CALENDAR_COLS}"
        ))
        .bind(id)
        .bind(workspace_id.into_uuid())
        .bind(connection_id.into_uuid())
        .bind(external_id)
        .bind(name)
        .bind(read_only)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Get-or-create a **local** (database-native) calendar — one with no
    /// provider connection (`connection_id IS NULL`) that is read-write and
    /// never synced (SOUL §8/§11). Idempotent per `(workspace_id, external_id)`
    /// via the local partial unique index (migration `0018`): a repeated call
    /// with the same `external_id` returns the same calendar with its `name`
    /// refreshed, so a default calendar (`external_id = "default"`) is safe to
    /// ensure on every write. Always `read_only = false`.
    pub async fn upsert_local(
        &self,
        workspace_id: WorkspaceId,
        external_id: &str,
        name: &str,
    ) -> Result<Calendar> {
        let id = CalendarId::new().into_uuid();
        let row: CalendarRow = sqlx::query_as(&format!(
            "INSERT INTO calendars (id, workspace_id, connection_id, external_id, name, read_only)
             VALUES ($1, $2, NULL, $3, $4, FALSE)
             ON CONFLICT (workspace_id, external_id) WHERE connection_id IS NULL
             DO UPDATE SET name = EXCLUDED.name
             RETURNING {CALENDAR_COLS}"
        ))
        .bind(id)
        .bind(workspace_id.into_uuid())
        .bind(external_id)
        .bind(name)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch a calendar, scoped to its workspace.
    pub async fn get(&self, workspace_id: WorkspaceId, id: CalendarId) -> Result<Calendar> {
        let row: CalendarRow = sqlx::query_as(&format!(
            "SELECT {CALENDAR_COLS} FROM calendars
             WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// List the calendars belonging to a connection, workspace-scoped.
    pub async fn list_by_connection(
        &self,
        workspace_id: WorkspaceId,
        connection_id: ConnectionId,
    ) -> Result<Vec<Calendar>> {
        let rows: Vec<CalendarRow> = sqlx::query_as(&format!(
            "SELECT {CALENDAR_COLS} FROM calendars
             WHERE workspace_id = $1 AND connection_id = $2
             ORDER BY name ASC"
        ))
        .bind(workspace_id.into_uuid())
        .bind(connection_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Calendar::from).collect())
    }

    /// List all calendars in a workspace.
    pub async fn list_by_workspace(&self, workspace_id: WorkspaceId) -> Result<Vec<Calendar>> {
        let rows: Vec<CalendarRow> = sqlx::query_as(&format!(
            "SELECT {CALENDAR_COLS} FROM calendars
             WHERE workspace_id = $1
             ORDER BY name ASC"
        ))
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Calendar::from).collect())
    }

    /// Delete a calendar (cascades to its events), workspace-scoped.
    pub async fn delete(&self, workspace_id: WorkspaceId, id: CalendarId) -> Result<()> {
        let res = sqlx::query("DELETE FROM calendars WHERE id = $1 AND workspace_id = $2")
            .bind(id.into_uuid())
            .bind(workspace_id.into_uuid())
            .execute(&self.pool)
            .await
            .map_err(map)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// Record that a provider calendar `(connection_id, external_id)` was removed
    /// by the user, so the sync paths won't re-`upsert` it (migration `0057`).
    /// Idempotent (a repeated exclude is a no-op). The row is cleared by the FK
    /// cascade when its connection is deleted, so removing + re-adding a source
    /// resurfaces the calendar. See [`Self::excluded_external_ids`].
    pub async fn exclude(
        &self,
        workspace_id: WorkspaceId,
        connection_id: ConnectionId,
        external_id: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO calendar_exclusions (workspace_id, connection_id, external_id)
             VALUES ($1, $2, $3)
             ON CONFLICT (connection_id, external_id) DO NOTHING",
        )
        .bind(workspace_id.into_uuid())
        .bind(connection_id.into_uuid())
        .bind(external_id)
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(())
    }

    /// The `external_id`s a connection's sync must skip re-creating — the
    /// user-deleted provider calendars recorded by [`Self::exclude`]. Empty for a
    /// connection with no exclusions; both `catalerum-ingest` sync paths consult
    /// this before upserting a listed provider calendar.
    pub async fn excluded_external_ids(
        &self,
        workspace_id: WorkspaceId,
        connection_id: ConnectionId,
    ) -> Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT external_id FROM calendar_exclusions
             WHERE workspace_id = $1 AND connection_id = $2",
        )
        .bind(workspace_id.into_uuid())
        .bind(connection_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(|(e,)| e).collect())
    }
}

// ===========================================================================
// Buckets & objects (the storage catalogue, SOUL §9)
// ===========================================================================

/// The full set of `buckets` columns, in row order.
const BUCKET_COLS: &str = "id, workspace_id, connection_id, name, prefix, created_at";

/// CRUD for the `buckets` table (SOUL §6.1/§9). A bucket belongs to a
/// storage-kind [`Connection`] (mirrors calendars on a calendar connection);
/// [`ensure`](BucketRepo::ensure) is the get-or-create the storage REST surface
/// calls before cataloguing an upload.
#[derive(Clone, Debug)]
pub struct BucketRepo {
    pool: PgPool,
}

impl BucketRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Get-or-create a bucket by `(connection_id, name)`. Idempotent: on conflict
    /// the existing row (id preserved) is returned with its `prefix` refreshed, so
    /// repeatedly cataloguing into the same configured bucket never duplicates.
    pub async fn ensure(
        &self,
        workspace_id: WorkspaceId,
        connection_id: ConnectionId,
        name: &str,
        prefix: Option<&str>,
    ) -> Result<Bucket> {
        let id = BucketId::new().into_uuid();
        let row: BucketRow = sqlx::query_as(&format!(
            "INSERT INTO buckets (id, workspace_id, connection_id, name, prefix)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (connection_id, name)
             DO UPDATE SET prefix = EXCLUDED.prefix
             RETURNING {BUCKET_COLS}"
        ))
        .bind(id)
        .bind(workspace_id.into_uuid())
        .bind(connection_id.into_uuid())
        .bind(name)
        .bind(prefix)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch a bucket, scoped to its workspace.
    pub async fn get(&self, workspace_id: WorkspaceId, id: BucketId) -> Result<Bucket> {
        let row: BucketRow = sqlx::query_as(&format!(
            "SELECT {BUCKET_COLS} FROM buckets WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch a bucket by its `name`, scoped to its workspace. The name is the
    /// catalogue label a `StorageObject` trigger carries as `bucket`, so this is
    /// how a storage-change automation resolves a triggered file back to its
    /// bucket. Names are expected unique per workspace (the newest wins if not).
    pub async fn get_by_name(&self, workspace_id: WorkspaceId, name: &str) -> Result<Bucket> {
        let row: BucketRow = sqlx::query_as(&format!(
            "SELECT {BUCKET_COLS} FROM buckets WHERE workspace_id = $1 AND name = $2 \
             ORDER BY created_at DESC LIMIT 1"
        ))
        .bind(workspace_id.into_uuid())
        .bind(name)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// List a workspace's buckets, newest first.
    pub async fn list_by_workspace(&self, workspace_id: WorkspaceId) -> Result<Vec<Bucket>> {
        let rows: Vec<BucketRow> = sqlx::query_as(&format!(
            "SELECT {BUCKET_COLS} FROM buckets WHERE workspace_id = $1 ORDER BY created_at DESC"
        ))
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Bucket::from).collect())
    }
}

/// The full set of `objects` columns, in row order.
const OBJECT_COLS: &str = "id, workspace_id, bucket_id, key, size, content_type, etag, \
     last_modified, sha256, extracted_text_id, created_at, updated_at";

/// Default ceiling on how many catalogued objects one
/// [`ObjectRepo::list_by_workspace`] call returns. A bucket catalogued from an
/// external store can hold many thousands of objects; an unbounded list would
/// balloon the API payload. The list is bounded to this many (most-recently-
/// modified first); narrow it with a key `prefix`. Normal-sized catalogues fall
/// well under it and are unaffected.
pub const DEFAULT_OBJECT_LIMIT: i64 = 1000;

/// Default cap on [`ObjectRepo::search_text_in_workspace`] results.
pub const DEFAULT_OBJECT_SEARCH_LIMIT: i64 = 50;

/// One object-content search hit: which object matched + a short excerpt of its
/// §10 extracted text windowed around the match.
#[derive(Clone, Debug)]
pub struct ObjectTextHit {
    pub id: ObjectId,
    /// The bucket the object lives in — lets a caller resolve the `?store=` the
    /// hit should download from (content search spans every store, §9).
    pub bucket_id: BucketId,
    pub key: String,
    pub content_type: Option<String>,
    pub excerpt: String,
}

/// New/updated object metadata for [`ObjectRepo::upsert`]. The catalogued handle
/// for a stored blob; the bytes themselves live in the bucket (§14).
#[derive(Debug, Clone)]
pub struct UpsertObject<'a> {
    pub workspace_id: WorkspaceId,
    pub bucket_id: BucketId,
    pub key: &'a str,
    pub size: u64,
    pub content_type: Option<&'a str>,
    pub etag: Option<&'a str>,
    pub last_modified: DateTime<Utc>,
    pub sha256: Option<&'a str>,
}

/// CRUD for the `objects` table (SOUL §6.1/§9/§10) — the catalogued, queryable
/// metadata for stored objects. `upsert` is keyed on `(bucket_id, key)` so a
/// re-upload refreshes metadata and never duplicates (§3.4); blobs stay in the
/// bucket. Powers `query_structured`'s object lookups (§6.5) and later §10
/// chunk/embed/project ingestion (via `extracted_text_id`).
#[derive(Clone, Debug)]
pub struct ObjectRepo {
    pool: PgPool,
}

impl ObjectRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Catalogue (or re-catalogue) an object by `(bucket_id, key)`. Idempotent:
    /// on conflict, `size`/`content_type`/`etag`/`last_modified`/`sha256` are
    /// refreshed and the id is preserved (`extracted_text_id` is left untouched,
    /// so a re-upload doesn't drop the §10 ingest link).
    pub async fn upsert(&self, obj: &UpsertObject<'_>) -> Result<StoredObject> {
        let id = ObjectId::new().into_uuid();
        let row: ObjectRow = sqlx::query_as(&format!(
            "INSERT INTO objects
                 (id, workspace_id, bucket_id, key, size, content_type, etag, last_modified, sha256)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT (bucket_id, key) DO UPDATE SET
                 size = EXCLUDED.size,
                 content_type = EXCLUDED.content_type,
                 etag = EXCLUDED.etag,
                 last_modified = EXCLUDED.last_modified,
                 sha256 = EXCLUDED.sha256,
                 updated_at = CURRENT_TIMESTAMP
             RETURNING {OBJECT_COLS}"
        ))
        .bind(id)
        .bind(obj.workspace_id.into_uuid())
        .bind(obj.bucket_id.into_uuid())
        .bind(obj.key)
        .bind(obj.size as i64)
        .bind(obj.content_type)
        .bind(obj.etag)
        .bind(obj.last_modified)
        .bind(obj.sha256)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch a catalogued object by id, workspace-scoped.
    pub async fn get(&self, workspace_id: WorkspaceId, id: ObjectId) -> Result<StoredObject> {
        let row: ObjectRow = sqlx::query_as(&format!(
            "SELECT {OBJECT_COLS} FROM objects WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Link (or clear) the [`Document`] holding this object's extracted text
    /// (§10), workspace-scoped. Idempotent; returns the updated object.
    pub async fn set_extracted_text(
        &self,
        workspace_id: WorkspaceId,
        id: ObjectId,
        document_id: Option<DocumentId>,
    ) -> Result<StoredObject> {
        let row: ObjectRow = sqlx::query_as(&format!(
            "UPDATE objects SET extracted_text_id = $3, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2
             RETURNING {OBJECT_COLS}"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(document_id.map(DocumentId::into_uuid))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch a catalogued object by `(bucket_id, key)`, workspace-scoped.
    pub async fn get_by_key(
        &self,
        workspace_id: WorkspaceId,
        bucket_id: BucketId,
        key: &str,
    ) -> Result<StoredObject> {
        let row: ObjectRow = sqlx::query_as(&format!(
            "SELECT {OBJECT_COLS} FROM objects
             WHERE workspace_id = $1 AND bucket_id = $2 AND key = $3"
        ))
        .bind(workspace_id.into_uuid())
        .bind(bucket_id.into_uuid())
        .bind(key)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// List a workspace's catalogued objects, most-recently-modified first.
    /// List the workspace's catalogued objects, most-recently-modified first,
    /// optionally restricted to keys under `prefix` (a *literal* prefix — LIKE
    /// metacharacters in it are not special — empty = all). Bounded to `limit`
    /// rows (floored at 1, see [`DEFAULT_OBJECT_LIMIT`]) so a large catalogue
    /// can't return an unbounded set; the bound applies *after* the prefix
    /// filter, so you get the first `limit` matches, not `limit`-then-filtered.
    pub async fn list_by_workspace(
        &self,
        workspace_id: WorkspaceId,
        prefix: &str,
        limit: i64,
    ) -> Result<Vec<StoredObject>> {
        let rows: Vec<ObjectRow> = sqlx::query_as(&format!(
            "SELECT {OBJECT_COLS} FROM objects
             WHERE workspace_id = $1
               AND ($2 = '' OR starts_with(key, $2))
             ORDER BY last_modified DESC, key ASC
             LIMIT $3"
        ))
        .bind(workspace_id.into_uuid())
        .bind(prefix)
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(StoredObject::from).collect())
    }

    /// List the workspace's catalogued objects carrying **no labels** (SOUL
    /// §9/§11) — the storage twin of `EmailRepo::list_untagged_by_workspace`:
    /// the backlog feed for a scheduled "label the unlabelled files" sweep.
    /// Labels live in `object_labels` keyed by `(store, path)` while objects key
    /// by `(bucket_id, key)`, and the bucket→store-name mapping is config (the
    /// storage registry), not DB — so the caller passes it in as `bucket_stores`
    /// and the anti-join runs per bucket under its own store name. Filtered in
    /// SQL (`NOT EXISTS`) so old unlabelled files stay reachable however many
    /// labelled ones are newer; `prefix` optionally restricts to keys under a
    /// subdirectory (a *literal* prefix — empty = everywhere). Only **exact**
    /// path labels count: a label on an ancestor directory does not mark the
    /// files under it. Most-recently-modified first, bounded to `limit` (floored
    /// at 1); empty `bucket_stores` → empty (no query).
    #[cfg(not(feature = "sqlite"))]
    pub async fn list_unlabeled_by_workspace(
        &self,
        workspace_id: WorkspaceId,
        bucket_stores: &[(BucketId, String)],
        prefix: &str,
        limit: i64,
    ) -> Result<Vec<StoredObject>> {
        if bucket_stores.is_empty() {
            return Ok(Vec::new());
        }
        let (buckets, stores): (Vec<Uuid>, Vec<String>) = bucket_stores
            .iter()
            .map(|(b, s)| (b.into_uuid(), s.clone()))
            .unzip();
        let rows: Vec<ObjectRow> = sqlx::query_as(&format!(
            "SELECT {OBJECT_COLS} FROM objects o
             JOIN unnest($2::uuid[], $3::text[]) AS m(b_id, store_name)
               ON m.b_id = o.bucket_id
             WHERE o.workspace_id = $1
               AND ($4 = '' OR starts_with(o.key, $4))
               AND NOT EXISTS (
                   SELECT 1 FROM object_labels l
                   WHERE l.workspace_id = o.workspace_id
                     AND l.store = m.store_name
                     AND l.path = o.key)
             ORDER BY o.last_modified DESC, o.key ASC
             LIMIT $5"
        ))
        .bind(workspace_id.into_uuid())
        .bind(&buckets)
        .bind(&stores)
        .bind(prefix)
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(StoredObject::from).collect())
    }

    #[cfg(feature = "sqlite")]
    pub async fn list_unlabeled_by_workspace(
        &self,
        workspace_id: WorkspaceId,
        bucket_stores: &[(BucketId, String)],
        prefix: &str,
        limit: i64,
    ) -> Result<Vec<StoredObject>> {
        if bucket_stores.is_empty() {
            return Ok(Vec::new());
        }
        let mut query =
            sqlx::QueryBuilder::<sqlx::Sqlite>::new("WITH mapping(b_id, store_name) AS (VALUES ");
        {
            let mut rows = query.separated(", ");
            for (bucket, store) in bucket_stores {
                rows.push("(")
                    .push_bind(bucket.into_uuid())
                    .push(", ")
                    .push_bind(store)
                    .push_unseparated(")");
            }
        }
        query
            .push(format!(
                ") SELECT {OBJECT_COLS} FROM objects o \
                 JOIN mapping m ON m.b_id = o.bucket_id WHERE o.workspace_id = "
            ))
            .push_bind(workspace_id.into_uuid())
            .push(" AND (")
            .push_bind(prefix)
            .push(" = '' OR substr(o.key, 1, length(")
            .push_bind(prefix)
            .push(")) = ")
            .push_bind(prefix)
            .push(
                ") AND NOT EXISTS (SELECT 1 FROM object_labels l \
                   WHERE l.workspace_id = o.workspace_id AND l.store = m.store_name \
                     AND l.path = o.key) ORDER BY o.last_modified DESC, o.key ASC LIMIT ",
            )
            .push_bind(limit.max(1));
        let rows: Vec<ObjectRow> = query
            .build_query_as()
            .fetch_all(&self.pool)
            .await
            .map_err(map)?;
        Ok(rows.into_iter().map(StoredObject::from).collect())
    }

    /// List a single bucket's catalogued objects, by key.
    pub async fn list_by_bucket(
        &self,
        workspace_id: WorkspaceId,
        bucket_id: BucketId,
    ) -> Result<Vec<StoredObject>> {
        let rows: Vec<ObjectRow> = sqlx::query_as(&format!(
            "SELECT {OBJECT_COLS} FROM objects
             WHERE workspace_id = $1 AND bucket_id = $2
             ORDER BY key ASC"
        ))
        .bind(workspace_id.into_uuid())
        .bind(bucket_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(StoredObject::from).collect())
    }

    /// Search objects by the **content** of their §10 extracted text. Joins each
    /// object to its extracted-text document (`extracted_text_id`), matches `query`
    /// as a **literal case-insensitive substring** (`strpos` — not `LIKE`, so a
    /// user's `%`/`_` are literal), and returns a hit per object with a ~160-char
    /// **excerpt windowed around the match** (computed in SQL, so the payload stays
    /// small regardless of document size), newest-modified first. A blank query
    /// returns nothing; bounded by `limit` (floored at 1). Only ingested objects
    /// (those with extracted text) can match.
    pub async fn search_text_in_workspace(
        &self,
        workspace_id: WorkspaceId,
        query: &str,
        limit: i64,
    ) -> Result<Vec<ObjectTextHit>> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<(Uuid, Uuid, String, Option<String>, String)> = sqlx::query_as(
            "SELECT o.id, o.bucket_id, o.key, o.content_type,
                    substring(d.text FROM greatest(1, strpos(lower(d.text), lower($2)) - 60) FOR 160)
                        AS excerpt
             FROM objects o
             JOIN documents d ON d.id = o.extracted_text_id
             WHERE o.workspace_id = $1 AND strpos(lower(d.text), lower($2)) > 0
             ORDER BY o.last_modified DESC, o.key ASC
             LIMIT $3",
        )
        .bind(workspace_id.into_uuid())
        .bind(query)
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows
            .into_iter()
            .map(
                |(id, bucket_id, key, content_type, excerpt)| ObjectTextHit {
                    id: ObjectId::from_uuid(id),
                    bucket_id: BucketId::from_uuid(bucket_id),
                    key,
                    content_type,
                    excerpt,
                },
            )
            .collect())
    }

    /// Remove a catalogued object by `(bucket_id, key)`, workspace-scoped.
    /// Idempotent: removing a never-catalogued / already-removed key is a no-op
    /// (mirrors the backend's idempotent delete).
    pub async fn delete_by_key(
        &self,
        workspace_id: WorkspaceId,
        bucket_id: BucketId,
        key: &str,
    ) -> Result<()> {
        sqlx::query("DELETE FROM objects WHERE workspace_id = $1 AND bucket_id = $2 AND key = $3")
            .bind(workspace_id.into_uuid())
            .bind(bucket_id.into_uuid())
            .bind(key)
            .execute(&self.pool)
            .await
            .map_err(map)?;
        Ok(())
    }
}

// ===========================================================================
// Events
// ===========================================================================

/// The full set of `events` columns, in row order.
const EVENT_COLS: &str = "id, workspace_id, calendar_id, uid, starts_at, ends_at, all_day, \
     rrule, summary, location, body, attendees, labels, attachments, etag, sequence, \
     created_at, updated_at";

/// Default ceiling on how many events one [`EventRepo::list_by_workspace`] call
/// returns. A workspace that syncs a long-lived CalDAV calendar can accumulate
/// thousands of events; an unbounded list would balloon the API payload and the
/// client agenda. The list is bounded to this many by start (ascending);
/// callers that need a narrower view pass a date range. Normal-sized calendars
/// fall well under it and are unaffected.
pub const DEFAULT_EVENT_LIMIT: i64 = 2000;

/// An event to upsert. The id is assigned by the store on first insert and
/// preserved across updates (the conflict key is `(calendar_id, uid)`).
#[derive(Clone, Debug)]
pub struct UpsertEvent<'a> {
    /// Owning workspace.
    pub workspace_id: WorkspaceId,
    /// Owning calendar.
    pub calendar_id: CalendarId,
    /// iCalendar `UID` (stable across edits; the idempotency key).
    pub uid: &'a str,
    /// Event start.
    pub starts_at: DateTime<Utc>,
    /// Event end.
    pub ends_at: DateTime<Utc>,
    /// All-day flag.
    pub all_day: bool,
    /// RFC 5545 recurrence rule, verbatim.
    pub rrule: Option<&'a str>,
    /// Title / summary.
    pub summary: &'a str,
    /// Location.
    pub location: Option<&'a str>,
    /// Free-text description / body.
    pub body: Option<&'a str>,
    /// Resolved attendees as typed entity pointers.
    pub attendees: &'a [EntityRef],
    /// Category labels (iCalendar `CATEGORIES`).
    pub labels: &'a [String],
    /// File / image attachments (iCalendar `ATTACH`).
    pub attachments: &'a [Attachment],
    /// Provider ETag for idempotent incremental sync (SOUL §3.4).
    pub etag: Option<&'a str>,
    /// iCalendar `SEQUENCE` for conflict resolution.
    pub sequence: i32,
}

impl<'a> UpsertEvent<'a> {
    /// A minimal upsert: required fields only, everything else empty/default.
    #[must_use]
    pub fn new(
        workspace_id: WorkspaceId,
        calendar_id: CalendarId,
        uid: &'a str,
        summary: &'a str,
        starts_at: DateTime<Utc>,
        ends_at: DateTime<Utc>,
    ) -> Self {
        Self {
            workspace_id,
            calendar_id,
            uid,
            starts_at,
            ends_at,
            all_day: false,
            rrule: None,
            summary,
            location: None,
            body: None,
            attendees: &[],
            labels: &[],
            attachments: &[],
            etag: None,
            sequence: 0,
        }
    }
}

/// An inclusive-start / exclusive-end time window to filter events by.
#[derive(Clone, Copy, Debug, Default)]
pub struct DateRange {
    /// Lower bound: only events with `starts_at >= from` (if set).
    pub from: Option<DateTime<Utc>>,
    /// Upper bound: only events with `starts_at < to` (if set).
    pub to: Option<DateTime<Utc>>,
}

/// The editable fields of an existing event, for a direct user/agent edit on a
/// local calendar ([`EventRepo::update`]). The `uid`, `etag`, and `attendees`
/// are *not* edited here: `uid` is the immutable identity, and `etag`/attendee
/// resolution belong to the sync/graph paths, not a hand edit. `SEQUENCE` is
/// bumped automatically by the update.
#[derive(Clone, Debug)]
pub struct EventPatch<'a> {
    /// New start.
    pub starts_at: DateTime<Utc>,
    /// New end.
    pub ends_at: DateTime<Utc>,
    /// New all-day flag.
    pub all_day: bool,
    /// New title / summary.
    pub summary: &'a str,
    /// New location (cleared when `None`).
    pub location: Option<&'a str>,
    /// New free-text body (cleared when `None`).
    pub body: Option<&'a str>,
    /// New category labels (replaces the prior set; empty clears them).
    pub labels: &'a [String],
    /// New file / image attachments (replaces the prior set; empty clears them).
    pub attachments: &'a [Attachment],
    /// New RFC 5545 recurrence rule (cleared when `None`).
    pub rrule: Option<&'a str>,
}

/// CRUD for the `events` table (SOUL §6.1/§8/§10). Events upsert by
/// `(calendar_id, uid)` so incremental sync is idempotent (SOUL §3.4):
/// re-running a sync never duplicates and updates land in place.
#[derive(Clone, Debug)]
pub struct EventRepo {
    pool: PgPool,
}

impl EventRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Insert or update an event by `(calendar_id, uid)` (SOUL §3.4). On
    /// conflict every mutable field is refreshed and `updated_at` bumped; the
    /// row id is preserved. Returns the stored event.
    pub async fn upsert_by_uid(&self, event: &UpsertEvent<'_>) -> Result<Event> {
        let id = EventId::new().into_uuid();
        let row: EventRow = sqlx::query_as(&format!(
            "INSERT INTO events
                 (id, workspace_id, calendar_id, uid, starts_at, ends_at, all_day,
                  rrule, summary, location, body, attendees, labels, attachments, etag, sequence)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
             ON CONFLICT (calendar_id, uid) DO UPDATE SET
                 starts_at   = EXCLUDED.starts_at,
                 ends_at     = EXCLUDED.ends_at,
                 all_day     = EXCLUDED.all_day,
                 rrule       = EXCLUDED.rrule,
                 summary     = EXCLUDED.summary,
                 location    = EXCLUDED.location,
                 body        = EXCLUDED.body,
                 attendees   = EXCLUDED.attendees,
                 labels      = EXCLUDED.labels,
                 attachments = EXCLUDED.attachments,
                 etag        = EXCLUDED.etag,
                 sequence    = EXCLUDED.sequence,
                 updated_at  = CURRENT_TIMESTAMP
             RETURNING {EVENT_COLS}"
        ))
        .bind(id)
        .bind(event.workspace_id.into_uuid())
        .bind(event.calendar_id.into_uuid())
        .bind(event.uid)
        .bind(event.starts_at)
        .bind(event.ends_at)
        .bind(event.all_day)
        .bind(event.rrule)
        .bind(event.summary)
        .bind(event.location)
        .bind(event.body)
        .bind(Json(event.attendees.to_vec()))
        .bind(Json(event.labels.to_vec()))
        .bind(Json(event.attachments.to_vec()))
        .bind(event.etag)
        .bind(event.sequence)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Insert a brand-new event (a direct create on a local calendar, SOUL
    /// §8/§11), assigning it a fresh id. Unlike [`upsert_by_uid`] this never
    /// updates an existing row: the caller supplies a freshly-minted `uid`
    /// (e.g. a UUID), so the `(calendar_id, uid)` key cannot collide — a
    /// collision surfaces as [`StoreError::Conflict`] rather than silently
    /// overwriting. The target calendar's writability is enforced by the caller
    /// (the API/tool rejects a read-only / provider calendar).
    ///
    /// [`upsert_by_uid`]: EventRepo::upsert_by_uid
    pub async fn create(&self, event: &UpsertEvent<'_>) -> Result<Event> {
        let id = EventId::new().into_uuid();
        let row: EventRow = sqlx::query_as(&format!(
            "INSERT INTO events
                 (id, workspace_id, calendar_id, uid, starts_at, ends_at, all_day,
                  rrule, summary, location, body, attendees, labels, attachments, etag, sequence)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
             RETURNING {EVENT_COLS}"
        ))
        .bind(id)
        .bind(event.workspace_id.into_uuid())
        .bind(event.calendar_id.into_uuid())
        .bind(event.uid)
        .bind(event.starts_at)
        .bind(event.ends_at)
        .bind(event.all_day)
        .bind(event.rrule)
        .bind(event.summary)
        .bind(event.location)
        .bind(event.body)
        .bind(Json(event.attendees.to_vec()))
        .bind(Json(event.labels.to_vec()))
        .bind(Json(event.attachments.to_vec()))
        .bind(event.etag)
        .bind(event.sequence)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Apply a direct edit to an existing event by id (SOUL §8/§11),
    /// workspace-scoped. Bumps `SEQUENCE` and `updated_at`; the immutable `uid`
    /// is preserved. Returns [`StoreError::NotFound`] if no such event exists in
    /// the workspace. Caller enforces that the owning calendar is writable.
    pub async fn update(
        &self,
        workspace_id: WorkspaceId,
        id: EventId,
        patch: &EventPatch<'_>,
    ) -> Result<Event> {
        let row: EventRow = sqlx::query_as(&format!(
            "UPDATE events SET
                 starts_at   = $3,
                 ends_at     = $4,
                 all_day     = $5,
                 summary     = $6,
                 location    = $7,
                 body        = $8,
                 rrule       = $9,
                 labels      = $10,
                 attachments = $11,
                 sequence    = sequence + 1,
                 updated_at  = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2
             RETURNING {EVENT_COLS}"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(patch.starts_at)
        .bind(patch.ends_at)
        .bind(patch.all_day)
        .bind(patch.summary)
        .bind(patch.location)
        .bind(patch.body)
        .bind(patch.rrule)
        .bind(Json(patch.labels.to_vec()))
        .bind(Json(patch.attachments.to_vec()))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Delete a single event by its id, workspace-scoped (a direct delete on a
    /// local calendar). Returns [`StoreError::NotFound`] if no row was removed.
    pub async fn delete(&self, workspace_id: WorkspaceId, id: EventId) -> Result<()> {
        let res = sqlx::query("DELETE FROM events WHERE id = $1 AND workspace_id = $2")
            .bind(id.into_uuid())
            .bind(workspace_id.into_uuid())
            .execute(&self.pool)
            .await
            .map_err(map)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// Fetch an event, scoped to its workspace.
    pub async fn get(&self, workspace_id: WorkspaceId, id: EventId) -> Result<Event> {
        let row: EventRow = sqlx::query_as(&format!(
            "SELECT {EVENT_COLS} FROM events
             WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch an event by `(calendar_id, uid)`, workspace-scoped — the calendar twin of
    /// [`EmailRepo::get_by_uid`](crate::repo::EmailRepo::get_by_uid). `StoreError::NotFound`
    /// when no such event exists; used by `WriteEvent` to probe whether an upsert is a
    /// first write vs an idempotent redelivery of an already-stored event (SOUL §11/§29).
    pub async fn get_by_uid(
        &self,
        workspace_id: WorkspaceId,
        calendar_id: CalendarId,
        uid: &str,
    ) -> Result<Event> {
        let row: EventRow = sqlx::query_as(&format!(
            "SELECT {EVENT_COLS} FROM events
             WHERE workspace_id = $1 AND calendar_id = $2 AND uid = $3"
        ))
        .bind(workspace_id.into_uuid())
        .bind(calendar_id.into_uuid())
        .bind(uid)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// List a workspace's events, ordered by start time ascending, with an
    /// optional date range (`starts_at` window) and an optional single-calendar
    /// filter. The query is always workspace-filtered.
    ///
    /// `limit` bounds the result to the first `limit` events by start (a `limit`
    /// below 1 is floored to 1) — so an open-ended query against a long-lived
    /// calendar can't return an unbounded set (see [`DEFAULT_EVENT_LIMIT`]).
    /// Callers that want a specific slice (e.g. "soonest from now") pass a `from`
    /// bound and the count they need; reconciliation that must see *every* row
    /// passes `i64::MAX`.
    pub async fn list_by_workspace(
        &self,
        workspace_id: WorkspaceId,
        calendar_id: Option<CalendarId>,
        range: DateRange,
        limit: i64,
    ) -> Result<Vec<Event>> {
        // Bind every optional filter and gate it with an `IS NULL OR` clause so
        // the SQL text stays constant (no dynamic string building).
        let rows: Vec<EventRow> = sqlx::query_as(&format!(
            "SELECT {EVENT_COLS} FROM events
             WHERE workspace_id = $1
               AND ($2::uuid        IS NULL OR calendar_id = $2)
               AND ($3::timestamptz IS NULL OR starts_at  >= $3)
               AND ($4::timestamptz IS NULL OR starts_at  <  $4)
             ORDER BY starts_at ASC, id ASC
             LIMIT $5"
        ))
        .bind(workspace_id.into_uuid())
        .bind(calendar_id.map(CalendarId::into_uuid))
        .bind(range.from)
        .bind(range.to)
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Event::from).collect())
    }

    /// Search events by the **content** of their summary, location, body, or an
    /// attendee's display name — `query` matched as a **literal case-insensitive
    /// substring** (`strpos`, not `LIKE`, so a user's `%`/`_` are literal),
    /// workspace-scoped, `LIMIT`-bounded (floored at 1). Unlike
    /// [`Self::list_by_workspace`] the order is `starts_at` **descending** (most
    /// recent first) so past events are as reachable as future ones — "when did
    /// I last meet X" surfaces the latest match first; an optional [`DateRange`]
    /// still narrows the window. A blank query returns nothing (no "match
    /// everything"). The content-search complement to `list_by_workspace`'s
    /// date filtering — drives the `search_events` agent tool.
    pub async fn search_in_workspace(
        &self,
        workspace_id: WorkspaceId,
        query: &str,
        range: DateRange,
        limit: i64,
    ) -> Result<Vec<Event>> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        // Attendees are a JSONB array of `EntityRef`s; match only their
        // `display_name` values (not the raw JSON text, whose constant keys /
        // UUIDs would false-positive).
        let rows: Vec<EventRow> = sqlx::query_as(&format!(
            "SELECT {EVENT_COLS} FROM events
             WHERE workspace_id = $1
               AND ($3::timestamptz IS NULL OR starts_at >= $3)
               AND ($4::timestamptz IS NULL OR starts_at <  $4)
               AND (strpos(lower(summary), lower($2)) > 0
                 OR strpos(lower(coalesce(location, '')), lower($2)) > 0
                 OR strpos(lower(coalesce(body, '')), lower($2)) > 0
                 OR EXISTS (SELECT 1 FROM jsonb_array_elements(attendees) a
                            WHERE strpos(lower(coalesce(a->>'display_name', '')), lower($2)) > 0))
             ORDER BY starts_at DESC, id DESC
             LIMIT $5"
        ))
        .bind(workspace_id.into_uuid())
        .bind(query)
        .bind(range.from)
        .bind(range.to)
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Event::from).collect())
    }

    /// Delete every event of a calendar, workspace-scoped. Returns the count
    /// removed. Used when a calendar is reset / removed from sync.
    pub async fn delete_by_calendar(
        &self,
        workspace_id: WorkspaceId,
        calendar_id: CalendarId,
    ) -> Result<u64> {
        let res = sqlx::query("DELETE FROM events WHERE workspace_id = $1 AND calendar_id = $2")
            .bind(workspace_id.into_uuid())
            .bind(calendar_id.into_uuid())
            .execute(&self.pool)
            .await
            .map_err(map)?;
        Ok(res.rows_affected())
    }

    /// Delete a single event by its provider `uid` within a calendar,
    /// workspace-scoped. Used to apply provider deletions during incremental
    /// sync. Returns `true` if a row was removed.
    pub async fn delete_by_uid(
        &self,
        workspace_id: WorkspaceId,
        calendar_id: CalendarId,
        uid: &str,
    ) -> Result<bool> {
        let res = sqlx::query(
            "DELETE FROM events
             WHERE workspace_id = $1 AND calendar_id = $2 AND uid = $3",
        )
        .bind(workspace_id.into_uuid())
        .bind(calendar_id.into_uuid())
        .bind(uid)
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(res.rows_affected() > 0)
    }
}

// ===========================================================================
// Mailboxes & emails (read-only email ingest, SOUL §28)
// ===========================================================================

/// The full set of `mailboxes` columns, in row order.
const GRANT_COLS: &str = "id, workspace_id, name, capabilities, constraints, created_at";

/// CRUD for the `grants` table (SOUL §19) — named capability bundles a workspace
/// Owner/Admin defines. An automation runs *under* a grant (its attenuated
/// authority); the runtime enforcement that resolves an automation's grant into
/// its `ToolContext` capabilities is a follow-up slice. Workspace-scoped (§18).
#[derive(Clone, Debug)]
pub struct GrantRepo {
    pool: PgPool,
}

impl GrantRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create (or, on a name conflict, **replace**) a grant. Idempotent by
    /// `(workspace_id, name)` so re-defining a named grant refreshes its
    /// capabilities + constraints; the id is preserved.
    pub async fn upsert(
        &self,
        workspace_id: WorkspaceId,
        name: &str,
        capabilities: &[Capability],
        constraints: &Constraints,
    ) -> Result<Grant> {
        let id = GrantId::new().into_uuid();
        let row: GrantRow = sqlx::query_as(&format!(
            "INSERT INTO grants (id, workspace_id, name, capabilities, constraints)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (workspace_id, name)
             DO UPDATE SET capabilities = EXCLUDED.capabilities,
                           constraints  = EXCLUDED.constraints
             RETURNING {GRANT_COLS}"
        ))
        .bind(id)
        .bind(workspace_id.into_uuid())
        .bind(name)
        .bind(Json(capabilities.to_vec()))
        .bind(Json(constraints.clone()))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch a grant, scoped to its workspace.
    pub async fn get(&self, workspace_id: WorkspaceId, id: GrantId) -> Result<Grant> {
        let row: GrantRow = sqlx::query_as(&format!(
            "SELECT {GRANT_COLS} FROM grants WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// List a workspace's grants, by name.
    pub async fn list_by_workspace(&self, workspace_id: WorkspaceId) -> Result<Vec<Grant>> {
        let rows: Vec<GrantRow> = sqlx::query_as(&format!(
            "SELECT {GRANT_COLS} FROM grants WHERE workspace_id = $1 ORDER BY name ASC"
        ))
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Grant::from).collect())
    }

    /// Delete a grant, workspace-scoped. Returns whether a row was removed
    /// (idempotent). An automation referencing it has its `grant_id` nulled by the
    /// FK (`ON DELETE SET NULL`), falling back to its default authority.
    pub async fn delete(&self, workspace_id: WorkspaceId, id: GrantId) -> Result<bool> {
        let res = sqlx::query("DELETE FROM grants WHERE id = $1 AND workspace_id = $2")
            .bind(id.into_uuid())
            .bind(workspace_id.into_uuid())
            .execute(&self.pool)
            .await
            .map_err(map)?;
        Ok(res.rows_affected() > 0)
    }
}

const MAILBOX_COLS: &str =
    "id, workspace_id, connection_id, external_id, name, read_only, created_at";

/// CRUD for the `mailboxes` table (SOUL §6.1/§28). A mailbox belongs to an
/// email-kind [`Connection`]; `upsert` is keyed on `(connection_id, external_id)`
/// so re-listing the provider's folders never duplicates (mirrors `calendars`).
#[derive(Clone, Debug)]
pub struct MailboxRepo {
    pool: PgPool,
}

impl MailboxRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Upsert a mailbox by `(connection_id, external_id)`. On conflict the `name`
    /// and `read_only` flag are refreshed; the id is preserved. Idempotent (§3.4).
    pub async fn upsert(
        &self,
        workspace_id: WorkspaceId,
        connection_id: ConnectionId,
        external_id: &str,
        name: &str,
        read_only: bool,
    ) -> Result<Mailbox> {
        let id = MailboxId::new().into_uuid();
        let row: MailboxRow = sqlx::query_as(&format!(
            "INSERT INTO mailboxes (id, workspace_id, connection_id, external_id, name, read_only)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (connection_id, external_id)
             DO UPDATE SET name = EXCLUDED.name, read_only = EXCLUDED.read_only
             RETURNING {MAILBOX_COLS}"
        ))
        .bind(id)
        .bind(workspace_id.into_uuid())
        .bind(connection_id.into_uuid())
        .bind(external_id)
        .bind(name)
        .bind(read_only)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch a mailbox, scoped to its workspace.
    pub async fn get(&self, workspace_id: WorkspaceId, id: MailboxId) -> Result<Mailbox> {
        let row: MailboxRow = sqlx::query_as(&format!(
            "SELECT {MAILBOX_COLS} FROM mailboxes WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch many mailboxes by id in **one** query, workspace-scoped — the batched
    /// form of [`get`](Self::get) (mirrors `EmailRepo`/`WorkspaceRepo::get_many`).
    /// Used by `search_emails` to resolve just the mailboxes its hits reference
    /// without listing every mailbox in the workspace. An id absent in this
    /// workspace is silently omitted; empty `ids` → empty (no query).
    #[cfg(not(feature = "sqlite"))]
    pub async fn get_many(
        &self,
        workspace_id: WorkspaceId,
        ids: &[MailboxId],
    ) -> Result<Vec<Mailbox>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let uuids: Vec<Uuid> = ids.iter().map(MailboxId::as_uuid).collect();
        let rows: Vec<MailboxRow> = sqlx::query_as(&format!(
            "SELECT {MAILBOX_COLS} FROM mailboxes WHERE workspace_id = $1 AND id = ANY($2)"
        ))
        .bind(workspace_id.into_uuid())
        .bind(&uuids)
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Mailbox::from).collect())
    }

    #[cfg(feature = "sqlite")]
    pub async fn get_many(
        &self,
        workspace_id: WorkspaceId,
        ids: &[MailboxId],
    ) -> Result<Vec<Mailbox>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(format!(
            "SELECT {MAILBOX_COLS} FROM mailboxes WHERE workspace_id = "
        ));
        query
            .push_bind(workspace_id.into_uuid())
            .push(" AND id IN (");
        let mut values = query.separated(", ");
        for id in ids {
            values.push_bind(id.as_uuid());
        }
        values.push_unseparated(")");
        let rows: Vec<MailboxRow> = query
            .build_query_as()
            .fetch_all(&self.pool)
            .await
            .map_err(map)?;
        Ok(rows.into_iter().map(Mailbox::from).collect())
    }

    /// List a workspace's mailboxes.
    pub async fn list_by_workspace(&self, workspace_id: WorkspaceId) -> Result<Vec<Mailbox>> {
        let rows: Vec<MailboxRow> = sqlx::query_as(&format!(
            "SELECT {MAILBOX_COLS} FROM mailboxes WHERE workspace_id = $1 ORDER BY name ASC"
        ))
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Mailbox::from).collect())
    }
}

/// The full set of `emails` columns, in row order.
const EMAIL_COLS: &str = "id, workspace_id, mailbox_id, uid, message_id, from_addr, to_addrs, \
     cc_addrs, subject, received_at, body_text, body_html, has_attachments, flags, labels, raw_ref, \
     attachments, created_at, updated_at";

/// CRUD for the `emails` table (SOUL §6.1/§28/§10). Emails upsert by
/// `(mailbox_id, uid)` so incremental sync is idempotent (§3.4): re-running never
/// duplicates and edits (flag changes, re-fetch) land in place.
#[derive(Clone, Debug)]
pub struct EmailRepo {
    pool: PgPool,
}

impl EmailRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Insert or update an email by `(mailbox_id, uid)`. On conflict every mutable
    /// field is refreshed and `updated_at` bumped; the row id is preserved. The
    /// passed `email`'s `id` is used only on first insert. Returns the stored email.
    pub async fn upsert_by_uid(&self, email: &Email) -> Result<Email> {
        let row: EmailRow = sqlx::query_as(&format!(
            "INSERT INTO emails
                 (id, workspace_id, mailbox_id, uid, message_id, from_addr, to_addrs, cc_addrs,
                  subject, received_at, body_text, body_html, has_attachments, flags)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
             ON CONFLICT (mailbox_id, uid) DO UPDATE SET
                 message_id      = EXCLUDED.message_id,
                 from_addr       = EXCLUDED.from_addr,
                 to_addrs        = EXCLUDED.to_addrs,
                 cc_addrs        = EXCLUDED.cc_addrs,
                 subject         = EXCLUDED.subject,
                 received_at     = EXCLUDED.received_at,
                 body_text       = EXCLUDED.body_text,
                 body_html       = EXCLUDED.body_html,
                 has_attachments = EXCLUDED.has_attachments,
                 flags           = EXCLUDED.flags,
                 updated_at      = CURRENT_TIMESTAMP
             RETURNING {EMAIL_COLS}"
        ))
        .bind(email.id.into_uuid())
        .bind(email.workspace_id.into_uuid())
        .bind(email.mailbox_id.into_uuid())
        .bind(&email.uid)
        .bind(email.message_id.as_deref())
        .bind(Json(email.from.clone()))
        .bind(Json(email.to.clone()))
        .bind(Json(email.cc.clone()))
        .bind(&email.subject)
        .bind(email.received_at)
        .bind(email.body_text.as_deref())
        .bind(email.body_html.as_deref())
        .bind(email.has_attachments)
        .bind(Json(email.flags.clone()))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Record the object-storage key of an email's archived raw message
    /// (`mail/<id>.eml`, SOUL §9/§28). Set after the raw bytes are written to the
    /// storage backend; kept out of [`upsert_by_uid`] so a flag-only re-sync (which
    /// does not know the key) never clobbers an existing ref. A no-op if the email
    /// is absent.
    pub async fn set_raw_ref(
        &self,
        workspace_id: WorkspaceId,
        id: EmailId,
        raw_ref: &str,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE emails SET raw_ref = $1, updated_at = CURRENT_TIMESTAMP WHERE id = $2 AND workspace_id = $3",
        )
        .bind(raw_ref)
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(())
    }

    /// Record an email's archived attachment **references** (SOUL §9/§28/§29): the
    /// list of [`Attachment`]s pointing at the objects a `WriteEmail`-triggered
    /// archival wrote to the workspace's files store. Like [`set_raw_ref`], kept out
    /// of [`upsert_by_uid`] so a flag-only re-sync (which carries no refs) never
    /// clobbers them; a full replace, idempotent. A no-op if the email is absent.
    pub async fn set_attachments(
        &self,
        workspace_id: WorkspaceId,
        id: EmailId,
        attachments: &[Attachment],
    ) -> Result<()> {
        sqlx::query(
            "UPDATE emails SET attachments = $1, updated_at = CURRENT_TIMESTAMP \
             WHERE id = $2 AND workspace_id = $3",
        )
        .bind(Json(attachments.to_vec()))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(())
    }

    /// Replace an email's free-text `labels` (SOUL §11/§28) — the classifier verdict
    /// a `LabelEmail` automation action records. Idempotent (a full replace), and
    /// kept out of [`upsert_by_uid`] so a provider re-sync (which carries no verdict)
    /// never clobbers a label set. Returns the updated email; `StoreError::NotFound`
    /// if the email is absent (so a verdict for an unwritten message surfaces rather
    /// than silently no-op'ing).
    pub async fn set_labels(
        &self,
        workspace_id: WorkspaceId,
        id: EmailId,
        labels: &[String],
    ) -> Result<Email> {
        let row: EmailRow = sqlx::query_as(&format!(
            "UPDATE emails SET labels = $1, updated_at = CURRENT_TIMESTAMP \
             WHERE id = $2 AND workspace_id = $3 RETURNING {EMAIL_COLS}"
        ))
        .bind(Json(labels.to_vec()))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Set or clear an email's local `seen` flag (SOUL §28) — the read/unread
    /// state the workbench and the `MarkEmailRead` automation action mutate.
    /// **Local only**: catalerum never writes back to the provider (§14), so a
    /// provider re-sync that carries fresh flags may overwrite this. One
    /// statement (strip any case-variant `seen` token, then append the
    /// normalized one when marking read), so it is idempotent and never
    /// duplicates the token. Returns the updated email; `StoreError::NotFound`
    /// if the email is absent.
    pub async fn set_seen(
        &self,
        workspace_id: WorkspaceId,
        id: EmailId,
        seen: bool,
    ) -> Result<Email> {
        let row: EmailRow = sqlx::query_as(&format!(
            "UPDATE emails SET flags =
                 (SELECT COALESCE(jsonb_agg(tok), '[]'::jsonb)
                  FROM jsonb_array_elements_text(flags) AS tok
                  WHERE lower(tok) <> 'seen')
                 || CASE WHEN $3 THEN '[\"seen\"]'::jsonb ELSE '[]'::jsonb END,
             updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2 RETURNING {EMAIL_COLS}"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(seen)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch an email by id, workspace-scoped.
    pub async fn get(&self, workspace_id: WorkspaceId, id: EmailId) -> Result<Email> {
        let row: EmailRow = sqlx::query_as(&format!(
            "SELECT {EMAIL_COLS} FROM emails WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch many emails by id in **one** query, workspace-scoped — the batched form
    /// of [`get`](Self::get), used by `search_emails` to resolve a page of vector
    /// hits without an N+1 (was a `get` per hit). An id absent in this workspace (a
    /// foreign tenant's id, or one whose row was purged after the vector outlived it)
    /// is **silently omitted**, so the result may be shorter than `ids` and in
    /// arbitrary order — the caller maps by id. Empty `ids` → empty (no query).
    #[cfg(not(feature = "sqlite"))]
    pub async fn get_many(&self, workspace_id: WorkspaceId, ids: &[EmailId]) -> Result<Vec<Email>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let uuids: Vec<Uuid> = ids.iter().map(EmailId::as_uuid).collect();
        let rows: Vec<EmailRow> = sqlx::query_as(&format!(
            "SELECT {EMAIL_COLS} FROM emails WHERE workspace_id = $1 AND id = ANY($2)"
        ))
        .bind(workspace_id.into_uuid())
        .bind(&uuids)
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Email::from).collect())
    }

    #[cfg(feature = "sqlite")]
    pub async fn get_many(&self, workspace_id: WorkspaceId, ids: &[EmailId]) -> Result<Vec<Email>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(format!(
            "SELECT {EMAIL_COLS} FROM emails WHERE workspace_id = "
        ));
        query
            .push_bind(workspace_id.into_uuid())
            .push(" AND id IN (");
        let mut values = query.separated(", ");
        for id in ids {
            values.push_bind(id.as_uuid());
        }
        values.push_unseparated(")");
        let rows: Vec<EmailRow> = query
            .build_query_as()
            .fetch_all(&self.pool)
            .await
            .map_err(map)?;
        Ok(rows.into_iter().map(Email::from).collect())
    }

    /// Fetch an email by `(mailbox_id, uid)`, workspace-scoped.
    pub async fn get_by_uid(
        &self,
        workspace_id: WorkspaceId,
        mailbox_id: MailboxId,
        uid: &str,
    ) -> Result<Email> {
        let row: EmailRow = sqlx::query_as(&format!(
            "SELECT {EMAIL_COLS} FROM emails
             WHERE workspace_id = $1 AND mailbox_id = $2 AND uid = $3"
        ))
        .bind(workspace_id.into_uuid())
        .bind(mailbox_id.into_uuid())
        .bind(uid)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// List a workspace's emails, most-recently-received first (nulls last).
    pub async fn list_by_workspace(
        &self,
        workspace_id: WorkspaceId,
        limit: i64,
    ) -> Result<Vec<Email>> {
        let rows: Vec<EmailRow> = sqlx::query_as(&format!(
            "SELECT {EMAIL_COLS} FROM emails
             WHERE workspace_id = $1
             ORDER BY received_at DESC NULLS LAST, id ASC
             LIMIT $2"
        ))
        .bind(workspace_id.into_uuid())
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Email::from).collect())
    }

    /// List a workspace's emails carrying **no labels** (SOUL §11/§28) —
    /// most-recently-received first (nulls last). The backlog feed for a
    /// scheduled "classify untagged mail" automation: filtered in SQL (`labels
    /// = '[]'`, total since migration 0028 makes the column NOT NULL DEFAULT
    /// '[]') so untagged mail older than any recent-scan window is still
    /// reachable, and already-labelled messages never crowd the page.
    pub async fn list_untagged_by_workspace(
        &self,
        workspace_id: WorkspaceId,
        limit: i64,
    ) -> Result<Vec<Email>> {
        let rows: Vec<EmailRow> = sqlx::query_as(&format!(
            "SELECT {EMAIL_COLS} FROM emails
             WHERE workspace_id = $1 AND labels = '[]'::jsonb
             ORDER BY received_at DESC NULLS LAST, id ASC
             LIMIT $2"
        ))
        .bind(workspace_id.into_uuid())
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Email::from).collect())
    }

    /// List a single mailbox's emails, most-recently-received first.
    pub async fn list_by_mailbox(
        &self,
        workspace_id: WorkspaceId,
        mailbox_id: MailboxId,
        limit: i64,
    ) -> Result<Vec<Email>> {
        let rows: Vec<EmailRow> = sqlx::query_as(&format!(
            "SELECT {EMAIL_COLS} FROM emails
             WHERE workspace_id = $1 AND mailbox_id = $2
             ORDER BY received_at DESC NULLS LAST, id ASC
             LIMIT $3"
        ))
        .bind(workspace_id.into_uuid())
        .bind(mailbox_id.into_uuid())
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Email::from).collect())
    }

    /// List every email row sharing an RFC 5322 `message_id`, workspace-scoped
    /// (SOUL §29 — **cross-folder dedup**). With per-folder mailboxes the same logical
    /// message lands as one row per folder it appears in (each keyed by its own
    /// `(mailbox_id, uid)`, since deletion/flags are per-folder); this collapses those
    /// N folder-copies by their shared `Message-ID` so a caller — recall/search
    /// dedup, or the web grouping a thread — can treat them as one logical email.
    /// Served by the `emails_message_id_idx` index over `(workspace_id, message_id)`.
    /// A blank id never groups (such rows store `NULL`); an empty match → empty.
    /// Most-recently-received first (nulls last), then `id` for a stable tie-break.
    pub async fn list_by_message_id(
        &self,
        workspace_id: WorkspaceId,
        message_id: &str,
    ) -> Result<Vec<Email>> {
        let rows: Vec<EmailRow> = sqlx::query_as(&format!(
            "SELECT {EMAIL_COLS} FROM emails
             WHERE workspace_id = $1 AND message_id = $2
             ORDER BY received_at DESC NULLS LAST, id ASC"
        ))
        .bind(workspace_id.into_uuid())
        .bind(message_id)
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Email::from).collect())
    }

    /// For each of `message_ids`, the DISTINCT mailbox **names** a message with that
    /// RFC 5322 `Message-ID` is filed under, workspace-scoped — the page-scoped group
    /// query behind the inbox listing's cross-folder dedup annotation (SOUL §29). ONE
    /// query for the whole listed page (never one-per-row): the caller passes the page's
    /// distinct non-null `message_id`s and gets back, per id, the set of folders that
    /// message appears in, so each row can be annotated "also in N other folders" without
    /// an N+1. A message filed in a single folder maps to a one-element vec; an id with no
    /// rows is absent from the map. Names are de-duplicated and returned in ascending
    /// order. Empty input → empty (no query). Served by the `emails_message_id_idx` index
    /// over `(workspace_id, message_id)`.
    #[cfg(not(feature = "sqlite"))]
    pub async fn folders_by_message_id(
        &self,
        workspace_id: WorkspaceId,
        message_ids: &[String],
    ) -> Result<std::collections::HashMap<String, Vec<String>>> {
        if message_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT DISTINCT e.message_id, m.name
             FROM emails e
             JOIN mailboxes m ON m.id = e.mailbox_id AND m.workspace_id = e.workspace_id
             WHERE e.workspace_id = $1 AND e.message_id = ANY($2)
             ORDER BY e.message_id, m.name",
        )
        .bind(workspace_id.into_uuid())
        .bind(message_ids)
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        let mut out: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for (mid, name) in rows {
            out.entry(mid).or_default().push(name);
        }
        Ok(out)
    }

    #[cfg(feature = "sqlite")]
    pub async fn folders_by_message_id(
        &self,
        workspace_id: WorkspaceId,
        message_ids: &[String],
    ) -> Result<std::collections::HashMap<String, Vec<String>>> {
        if message_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
            "SELECT DISTINCT e.message_id, m.name FROM emails e \
             JOIN mailboxes m ON m.id = e.mailbox_id AND m.workspace_id = e.workspace_id \
             WHERE e.workspace_id = ",
        );
        query
            .push_bind(workspace_id.into_uuid())
            .push(" AND e.message_id IN (");
        let mut values = query.separated(", ");
        for id in message_ids {
            values.push_bind(id);
        }
        values.push_unseparated(") ORDER BY e.message_id, m.name");
        let rows: Vec<(String, String)> = query
            .build_query_as()
            .fetch_all(&self.pool)
            .await
            .map_err(map)?;
        let mut out: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for (mid, name) in rows {
            out.entry(mid).or_default().push(name);
        }
        Ok(out)
    }

    /// Per-mailbox **unread counts** for a whole workspace in ONE grouped query
    /// (SOUL §28) — the badge numbers next to each mailbox in the inbox sidebar.
    /// Unread mirrors `is_unread` (no case-insensitive `seen` flag). A mailbox
    /// with no unread mail is absent from the map (the caller treats missing as
    /// `0`).
    pub async fn unread_counts_by_mailbox(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<std::collections::HashMap<MailboxId, i64>> {
        let rows: Vec<(uuid::Uuid, i64)> = sqlx::query_as(
            "SELECT mailbox_id, count(*) FROM emails
             WHERE workspace_id = $1
               AND NOT EXISTS (
                   SELECT 1 FROM jsonb_array_elements_text(flags) AS tok
                   WHERE lower(tok) = 'seen'
               )
             GROUP BY mailbox_id",
        )
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows
            .into_iter()
            .map(|(id, n)| (MailboxId::from_uuid(id), n))
            .collect())
    }

    /// Search a workspace's emails by optional `content` (subject/body substring),
    /// `sender` (from address/name substring), and `unread` state, optionally scoped
    /// to one `mailbox`, most-recently-received first, bounded to `limit`.
    ///
    /// Every predicate is applied **in SQL** so the `LIMIT` bounds *matching* rows —
    /// unlike a scan-then-filter, a match that lives in older-than-`limit` mail is
    /// still found (SOUL §28/§18). Substring matching is case-insensitive and
    /// **literal** (`strpos`, not `ILIKE`, so a `%`/`_` in the term isn't a
    /// wildcard), mirroring the route's previous `contains` filter. The `unread`
    /// predicate mirrors `is_unread` (true ⟺ no case-insensitive `seen` flag).
    /// `None` for any predicate means "no constraint".
    pub async fn search_in_workspace(
        &self,
        workspace_id: WorkspaceId,
        mailbox: Option<MailboxId>,
        content: Option<&str>,
        sender: Option<&str>,
        unread: Option<bool>,
        limit: i64,
    ) -> Result<Vec<Email>> {
        let rows: Vec<EmailRow> = sqlx::query_as(&format!(
            "SELECT {EMAIL_COLS} FROM emails
             WHERE workspace_id = $1
               AND ($2::uuid IS NULL OR mailbox_id = $2)
               AND ($3::text IS NULL
                    OR strpos(lower(subject), lower($3)) > 0
                    OR strpos(lower(coalesce(body_text, '')), lower($3)) > 0)
               AND ($4::text IS NULL
                    OR strpos(lower(coalesce(from_addr->>'address', '')), lower($4)) > 0
                    OR strpos(lower(coalesce(from_addr->>'name', '')), lower($4)) > 0)
               AND ($5::bool IS NULL
                    OR (NOT EXISTS (
                        SELECT 1 FROM jsonb_array_elements_text(flags) AS tok
                        WHERE lower(tok) = 'seen'
                    )) = $5)
             ORDER BY received_at DESC NULLS LAST, id ASC
             LIMIT $6"
        ))
        .bind(workspace_id.into_uuid())
        .bind(mailbox.map(MailboxId::into_uuid))
        .bind(content)
        .bind(sender)
        .bind(unread)
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Email::from).collect())
    }

    /// Delete an email by `(mailbox_id, uid)`, workspace-scoped. Returns the
    /// deleted email's id (so sync can enqueue a projection purge for it, §10) or
    /// `None` if no row matched. Idempotent.
    pub async fn delete_by_uid(
        &self,
        workspace_id: WorkspaceId,
        mailbox_id: MailboxId,
        uid: &str,
    ) -> Result<Option<EmailId>> {
        let row: Option<(uuid::Uuid,)> = sqlx::query_as(
            "DELETE FROM emails WHERE workspace_id = $1 AND mailbox_id = $2 AND uid = $3
             RETURNING id",
        )
        .bind(workspace_id.into_uuid())
        .bind(mailbox_id.into_uuid())
        .bind(uid)
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.map(|(id,)| EmailId::from_uuid(id)))
    }
}

// ===========================================================================
// Job queue
// ===========================================================================

/// The full set of `job_queue` columns, in row order.
const JOB_COLS: &str = "id, workspace_id, kind, payload, status, attempts, run_after, \
     locked_at, locked_by, last_error, created_at, updated_at";

/// CRUD + lease semantics for the durable `job_queue` (SOUL §6.2).
///
/// All async work is enqueued here first; a worker claims one runnable job via
/// `SELECT … FOR UPDATE SKIP LOCKED` so concurrent workers never grab the same
/// row. A claimed job is `complete`d on success or `fail`ed with exponential
/// backoff (re-`pending` until `max_attempts`, then terminal `failed`). The
/// single-pod dev path uses exactly this `FOR UPDATE SKIP LOCKED` queue (Valkey
/// disabled, SOUL §6.2). A worker that dies mid-job leaves its row `running`
/// forever; [`Self::reclaim_stale`] is the reconciler that re-drives such
/// orphaned leases past a visibility timeout, so a crash loses throughput, never
/// work (SOUL §6.2).
#[derive(Clone, Debug)]
pub struct JobQueueRepo {
    pool: PgPool,
}

impl JobQueueRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Enqueue a job. `workspace_id` is `None` for global maintenance jobs;
    /// `run_after` defaults to `CURRENT_TIMESTAMP` (eligible immediately) when `None`.
    /// Returns the stored job row.
    pub async fn enqueue(
        &self,
        workspace_id: Option<WorkspaceId>,
        kind: &str,
        payload: serde_json::Value,
        run_after: Option<DateTime<Utc>>,
    ) -> Result<JobRow> {
        let id = Uuid::new_v4();
        let row: JobRow = sqlx::query_as(&format!(
            "INSERT INTO job_queue (id, workspace_id, kind, payload, run_after)
             VALUES ($1, $2, $3, $4, COALESCE($5, CURRENT_TIMESTAMP))
             RETURNING {JOB_COLS}"
        ))
        .bind(id)
        .bind(workspace_id.map(WorkspaceId::into_uuid))
        .bind(kind)
        .bind(Json(payload))
        .bind(run_after)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row)
    }

    /// Atomically claim the oldest runnable pending job and mark it `running`,
    /// leased to `worker`. Uses `FOR UPDATE SKIP LOCKED` so concurrent workers
    /// never claim the same row (SOUL §6.2). Returns `None` when the queue is
    /// empty / nothing is yet eligible (`run_after <= CURRENT_TIMESTAMP`).
    pub async fn dequeue_one(&self, worker: &str) -> Result<Option<JobRow>> {
        #[cfg(not(feature = "sqlite"))]
        let candidate_lock = "FOR UPDATE SKIP LOCKED";
        // SQLite serializes writers. The UPDATE-with-subquery is atomic and is
        // therefore the native equivalent for the single-node configuration.
        #[cfg(feature = "sqlite")]
        let candidate_lock = "";
        #[cfg(not(feature = "sqlite"))]
        let runnable = "run_after <= CURRENT_TIMESTAMP";
        #[cfg(feature = "sqlite")]
        let runnable = "julianday(run_after) <= julianday(CURRENT_TIMESTAMP)";
        let row: Option<JobRow> = sqlx::query_as(&format!(
            "UPDATE job_queue SET
                 status     = 'running',
                 attempts   = attempts + 1,
                 locked_at  = CURRENT_TIMESTAMP,
                 locked_by  = $1,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = (
                 SELECT id FROM job_queue
                 WHERE status = 'pending' AND {runnable}
                 ORDER BY run_after ASC, created_at ASC
                 {candidate_lock}
                 LIMIT 1
             )
             RETURNING {JOB_COLS}"
        ))
        .bind(worker)
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        Ok(row)
    }

    /// Fetch a job by id (any status), or [`StoreError::NotFound`].
    pub async fn get(&self, id: Uuid) -> Result<JobRow> {
        let row: JobRow =
            sqlx::query_as(&format!("SELECT {JOB_COLS} FROM job_queue WHERE id = $1"))
                .bind(id)
                .fetch_one(&self.pool)
                .await
                .map_err(map)?;
        Ok(row)
    }

    /// Mark a running job `done` (terminal, success) and release its lease.
    /// Returns the updated row, or [`StoreError::NotFound`] if the id is unknown.
    pub async fn complete(&self, id: Uuid) -> Result<JobRow> {
        let row: JobRow = sqlx::query_as(&format!(
            "UPDATE job_queue SET
                 status     = 'done',
                 locked_at  = NULL,
                 locked_by  = NULL,
                 last_error = NULL,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = $1
             RETURNING {JOB_COLS}"
        ))
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row)
    }

    /// Fail a job, recording `error`. If `attempts < max_attempts` the job is
    /// re-queued (`pending`) with exponential backoff — `run_after = CURRENT_TIMESTAMP +
    /// backoff_base * 2^(attempts-1)` — so a transient failure retries later;
    /// otherwise it becomes terminal `failed`. The lease is released either way.
    /// Returns the updated row.
    pub async fn fail(
        &self,
        id: Uuid,
        error: &str,
        max_attempts: i32,
        backoff_base: Duration,
    ) -> Result<JobRow> {
        #[cfg(not(feature = "sqlite"))]
        let backoff_secs = backoff_base.as_secs_f64().max(0.0);
        #[cfg(not(feature = "sqlite"))]
        let row: JobRow = sqlx::query_as(&format!(
            "UPDATE job_queue SET
                 status     = CASE WHEN attempts >= $2 THEN 'failed' ELSE 'pending' END,
                 run_after  = CASE WHEN attempts >= $2 THEN run_after
                                   ELSE CURRENT_TIMESTAMP + make_interval(
                                       secs => $4 * power(2, GREATEST(attempts - 1, 0)))
                              END,
                 locked_at  = NULL,
                 locked_by  = NULL,
                 last_error = $3,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = $1
             RETURNING {JOB_COLS}"
        ))
        .bind(id)
        .bind(max_attempts)
        .bind(error)
        .bind(backoff_secs)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        #[cfg(feature = "sqlite")]
        let row: JobRow = {
            let (attempts,): (i32,) =
                sqlx::query_as("SELECT attempts FROM job_queue WHERE id = $1")
                    .bind(id)
                    .fetch_one(&self.pool)
                    .await
                    .map_err(map)?;
            let exponent = u32::try_from((attempts - 1).clamp(0, 31)).unwrap_or(0);
            let delay = backoff_base.saturating_mul(1_u32 << exponent);
            let retry_at = Utc::now()
                .checked_add_signed(
                    chrono::Duration::from_std(delay).unwrap_or(chrono::Duration::MAX),
                )
                .unwrap_or(DateTime::<Utc>::MAX_UTC);
            sqlx::query_as(&format!(
                "UPDATE job_queue SET
                     status     = CASE WHEN attempts >= $2 THEN 'failed' ELSE 'pending' END,
                     run_after  = CASE WHEN attempts >= $2 THEN run_after ELSE datetime($4) END,
                     locked_at  = NULL,
                     locked_by  = NULL,
                     last_error = $3,
                     updated_at = CURRENT_TIMESTAMP
                 WHERE id = $1
                 RETURNING {JOB_COLS}"
            ))
            .bind(id)
            .bind(max_attempts)
            .bind(error)
            .bind(retry_at)
            .fetch_one(&self.pool)
            .await
            .map_err(map)?
        };
        Ok(row)
    }

    /// Reconcile stale leases (SOUL §6.2): re-drive any job stuck `running` whose
    /// lease has expired — `locked_at < CURRENT_TIMESTAMP - visibility_timeout`. A worker that
    /// claims a job (so `dequeue_one` flipped it to `running` and bumped
    /// `attempts`) and then dies before [`Self::complete`]/[`Self::fail`] leaves
    /// the row `running` forever; this is the reconciler that re-drives it so a
    /// crash loses throughput, never work.
    ///
    /// A reclaimed job whose `attempts` is still below `max_attempts` returns to
    /// `pending`, eligible immediately (`run_after = CURRENT_TIMESTAMP`), lease released; one
    /// that has already exhausted its attempts becomes terminal `failed` (so a
    /// job that reliably crashes its worker cannot be reclaimed forever — the
    /// crashed claim already consumed an attempt). `last_error` records the
    /// reclaim. Returns the number of jobs reclaimed.
    ///
    /// The single `UPDATE` is atomic, so running this from several workers (or a
    /// dedicated reconciler) never double-drives a row. `visibility_timeout` must
    /// exceed the longest expected job runtime, or a still-running job is reclaimed
    /// and run twice (harmless only for idempotent jobs, SOUL §3.4).
    pub async fn reclaim_stale(
        &self,
        visibility_timeout: Duration,
        max_attempts: i32,
    ) -> Result<u64> {
        let cutoff = cutoff_before(visibility_timeout);
        #[cfg(not(feature = "sqlite"))]
        let expired = "locked_at < $1";
        #[cfg(feature = "sqlite")]
        let expired = "julianday(locked_at) < julianday($1)";
        let res = sqlx::query(&format!(
            "UPDATE job_queue SET
                 status     = CASE WHEN attempts >= $2 THEN 'failed' ELSE 'pending' END,
                 run_after  = CASE WHEN attempts >= $2 THEN run_after ELSE CURRENT_TIMESTAMP END,
                 locked_at  = NULL,
                 locked_by  = NULL,
                 last_error = $3,
                 updated_at = CURRENT_TIMESTAMP
             WHERE status = 'running'
               AND locked_at IS NOT NULL
               AND {expired}"
        ))
        .bind(cutoff)
        .bind(max_attempts)
        .bind("reclaimed: worker lease expired past visibility timeout")
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(res.rows_affected())
    }

    /// Count jobs in a given lifecycle status (across all workspaces). Handy for
    /// metrics / draining checks.
    pub async fn count_by_status(&self, status: JobStatus) -> Result<i64> {
        let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM job_queue WHERE status = $1")
            .bind(status.as_str())
            .fetch_one(&self.pool)
            .await
            .map_err(map)?;
        Ok(count)
    }

    /// Garbage-collect terminal (`done`/`failed`) jobs older than `before`.
    /// Returns the number deleted.
    pub async fn delete_terminal_before(&self, before: DateTime<Utc>) -> Result<u64> {
        #[cfg(not(feature = "sqlite"))]
        let older = "updated_at < $1";
        #[cfg(feature = "sqlite")]
        let older = "julianday(updated_at) < julianday($1)";
        let res = sqlx::query(&format!(
            "DELETE FROM job_queue
             WHERE status IN ('done', 'failed') AND {older}"
        ))
        .bind(before)
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(res.rows_affected())
    }
}

// ===========================================================================
// Documents & chunks (ingest derivation, SOUL §5/§6.4/§10)
// ===========================================================================

/// The full set of `documents` columns, in row order (for `SELECT`/`RETURNING`).
const DOCUMENT_COLS: &str =
    "id, workspace_id, source_kind, source_id, text, summary, created_at, updated_at";

/// CRUD for the `documents` table — extracted text for one source artifact, the
/// unit that gets chunked + embedded (SOUL §10). Keyed on the core `SourceRef`
/// so ingesting the same source twice upserts one stable row.
#[derive(Clone, Debug)]
pub struct DocumentRepo {
    pool: PgPool,
}

impl DocumentRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Idempotently upsert the document for `source` in a workspace, replacing
    /// its `text`/`summary` and bumping `updated_at`. The document **id is
    /// stable** across upserts (the `(workspace_id, source_kind, source_id)`
    /// unique key), so a note's chunks/edges keep referring to the same document
    /// on every re-ingest (SOUL §3.4/§10).
    pub async fn upsert_by_source(
        &self,
        workspace_id: WorkspaceId,
        source: &SourceRef,
        text: &str,
        summary: Option<&str>,
    ) -> Result<Document> {
        let (kind, sid) = source_to_parts(source);
        let row: DocumentRow = sqlx::query_as(&format!(
            "INSERT INTO documents (id, workspace_id, source_kind, source_id, text, summary)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (workspace_id, source_kind, source_id)
             DO UPDATE SET text = EXCLUDED.text,
                           summary = EXCLUDED.summary,
                           updated_at = CURRENT_TIMESTAMP
             RETURNING {DOCUMENT_COLS}"
        ))
        .bind(DocumentId::new().into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(kind)
        .bind(sid)
        .bind(text)
        .bind(summary)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Fetch the document for `source`, or `None` if it has not been ingested.
    pub async fn get_by_source(
        &self,
        workspace_id: WorkspaceId,
        source: &SourceRef,
    ) -> Result<Option<Document>> {
        let (kind, sid) = source_to_parts(source);
        let row: Option<DocumentRow> = sqlx::query_as(&format!(
            "SELECT {DOCUMENT_COLS} FROM documents
             WHERE workspace_id = $1 AND source_kind = $2 AND source_id = $3"
        ))
        .bind(workspace_id.into_uuid())
        .bind(kind)
        .bind(sid)
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        row.map(Document::try_from).transpose()
    }

    /// Delete the document for `source` (cascading to its chunks), workspace-
    /// scoped. Returns whether a row was deleted (a no-op if absent).
    pub async fn delete_by_source(
        &self,
        workspace_id: WorkspaceId,
        source: &SourceRef,
    ) -> Result<bool> {
        let (kind, sid) = source_to_parts(source);
        let res = sqlx::query(
            "DELETE FROM documents
             WHERE workspace_id = $1 AND source_kind = $2 AND source_id = $3",
        )
        .bind(workspace_id.into_uuid())
        .bind(kind)
        .bind(sid)
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(res.rows_affected() > 0)
    }
}

/// A chunk to insert: its position, text, and (optional) Qdrant point handle.
#[derive(Clone, Debug)]
pub struct NewChunk {
    /// 0-based position within the document.
    pub ordinal: i32,
    /// The chunk text.
    pub text: String,
    /// The Qdrant point id this chunk was (or will be) upserted as.
    pub point_id: Option<Uuid>,
}

impl NewChunk {
    /// A chunk at `ordinal` holding `text`, embedded as point `point_id`.
    #[must_use]
    pub fn new(ordinal: i32, text: impl Into<String>, point_id: Option<Uuid>) -> Self {
        Self {
            ordinal,
            text: text.into(),
            point_id,
        }
    }
}

/// The full set of `chunks` columns, in row order (for `SELECT`/`RETURNING`).
const CHUNK_COLS: &str = "id, workspace_id, document_id, ordinal, text, point_id, created_at";

/// CRUD for the `chunks` table — the embedded slices of a document (SOUL §6.4).
#[derive(Clone, Debug)]
pub struct ChunkRepo {
    pool: PgPool,
}

impl ChunkRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Replace **all** of a document's chunks in one transaction: delete the old
    /// set, insert `chunks` in order. This is the re-chunk basis — a document's
    /// chunk list is regenerated wholesale on every re-ingest, so it never
    /// drifts (SOUL §3.4/§10). Returns the stored chunks in `ordinal` order.
    /// `document_id` is verified to live in `workspace_id` first, so a caller
    /// cannot write chunks across the tenancy boundary (§18).
    pub async fn replace_for_document(
        &self,
        workspace_id: WorkspaceId,
        document_id: DocumentId,
        chunks: &[NewChunk],
    ) -> Result<Vec<Chunk>> {
        let mut tx = self.pool.begin().await.map_err(map)?;

        // Tenancy guard: the document must belong to this workspace.
        let owner: Option<(Uuid,)> =
            sqlx::query_as("SELECT workspace_id FROM documents WHERE id = $1")
                .bind(document_id.into_uuid())
                .fetch_optional(&mut *tx)
                .await
                .map_err(map)?;
        match owner {
            Some((ws,)) if ws == workspace_id.into_uuid() => {}
            _ => return Err(StoreError::NotFound),
        }

        sqlx::query("DELETE FROM chunks WHERE document_id = $1")
            .bind(document_id.into_uuid())
            .execute(&mut *tx)
            .await
            .map_err(map)?;

        let mut stored = Vec::with_capacity(chunks.len());
        for c in chunks {
            let row: ChunkRow = sqlx::query_as(&format!(
                "INSERT INTO chunks (id, workspace_id, document_id, ordinal, text, point_id)
                 VALUES ($1, $2, $3, $4, $5, $6)
                 RETURNING {CHUNK_COLS}"
            ))
            .bind(ChunkId::new().into_uuid())
            .bind(workspace_id.into_uuid())
            .bind(document_id.into_uuid())
            .bind(c.ordinal)
            .bind(&c.text)
            .bind(c.point_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(map)?;
            stored.push(row.into());
        }

        tx.commit().await.map_err(map)?;
        Ok(stored)
    }

    /// List a document's chunks in `ordinal` order, workspace-scoped.
    pub async fn list_by_document(
        &self,
        workspace_id: WorkspaceId,
        document_id: DocumentId,
    ) -> Result<Vec<Chunk>> {
        let rows: Vec<ChunkRow> = sqlx::query_as(&format!(
            "SELECT {CHUNK_COLS} FROM chunks
             WHERE workspace_id = $1 AND document_id = $2
             ORDER BY ordinal ASC"
        ))
        .bind(workspace_id.into_uuid())
        .bind(document_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Chunk::from).collect())
    }

    /// Count a workspace's chunks (across all documents).
    pub async fn count_by_workspace(&self, workspace_id: WorkspaceId) -> Result<i64> {
        let (count,): (i64,) =
            sqlx::query_as("SELECT count(*) FROM chunks WHERE workspace_id = $1")
                .bind(workspace_id.into_uuid())
                .fetch_one(&self.pool)
                .await
                .map_err(map)?;
        Ok(count)
    }
}

// ===========================================================================
// Memories (personalization, SOUL §22)
// ===========================================================================

/// The full set of `memories` columns, in row order (for `SELECT`/`RETURNING`).
const MEMORY_COLS: &str =
    "id, workspace_id, scope, user_id, text, source_kind, source_id, point_id, created_at, updated_at";

/// CRUD for the `memories` table — durable, curated, inspectable facts (SOUL §22).
/// `scope` is `User` (private to one member) or `Workspace` (shared); recall
/// applies the user-visibility filter so a member never sees another member's
/// private memories.
#[derive(Clone, Debug)]
pub struct MemoryRepo {
    pool: PgPool,
}

impl MemoryRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create a memory in a workspace. `user_id` is required for `User` scope
    /// (the member it is private to) and ignored for `Workspace` scope. `source`
    /// optionally records where it was derived from (e.g. a conversation).
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        scope: MemoryScope,
        user_id: Option<UserId>,
        text: &str,
        source: Option<&SourceRef>,
    ) -> Result<Memory> {
        // A workspace-scoped memory is never tied to a member.
        let user_id = match scope {
            MemoryScope::User => user_id,
            MemoryScope::Workspace => None,
        };
        let (source_kind, source_id) = match source {
            Some(s) => {
                let (k, i) = source_to_parts(s);
                (Some(k), Some(i))
            }
            None => (None, None),
        };
        let row: MemoryRow = sqlx::query_as(&format!(
            "INSERT INTO memories
                 (id, workspace_id, scope, user_id, text, source_kind, source_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             RETURNING {MEMORY_COLS}"
        ))
        .bind(MemoryId::new().into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(memory_scope_to_text(scope))
        .bind(user_id.map(UserId::into_uuid))
        .bind(text)
        .bind(source_kind)
        .bind(source_id)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Fetch a memory, scoped to its workspace.
    pub async fn get(&self, workspace_id: WorkspaceId, id: MemoryId) -> Result<Memory> {
        let row: MemoryRow = sqlx::query_as(&format!(
            "SELECT {MEMORY_COLS} FROM memories WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Fetch multiple memories by id in **one** round-trip, workspace-scoped. Ids
    /// with no matching row are simply absent (no error) and order is unspecified,
    /// so callers index the result by id. Lets a batch of semantic-search hits be
    /// visibility-filtered without a `get` per hit on the hot chat-recall path.
    #[cfg(not(feature = "sqlite"))]
    pub async fn get_many(
        &self,
        workspace_id: WorkspaceId,
        ids: &[MemoryId],
    ) -> Result<Vec<Memory>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let uuids: Vec<Uuid> = ids.iter().map(MemoryId::as_uuid).collect();
        let rows: Vec<MemoryRow> = sqlx::query_as(&format!(
            "SELECT {MEMORY_COLS} FROM memories WHERE workspace_id = $1 AND id = ANY($2)"
        ))
        .bind(workspace_id.into_uuid())
        .bind(&uuids)
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(Memory::try_from).collect()
    }

    #[cfg(feature = "sqlite")]
    pub async fn get_many(
        &self,
        workspace_id: WorkspaceId,
        ids: &[MemoryId],
    ) -> Result<Vec<Memory>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(format!(
            "SELECT {MEMORY_COLS} FROM memories WHERE workspace_id = "
        ));
        query
            .push_bind(workspace_id.into_uuid())
            .push(" AND id IN (");
        let mut values = query.separated(", ");
        for id in ids {
            values.push_bind(id.as_uuid());
        }
        values.push_unseparated(")");
        let rows: Vec<MemoryRow> = query
            .build_query_as()
            .fetch_all(&self.pool)
            .await
            .map_err(map)?;
        rows.into_iter().map(Memory::try_from).collect()
    }

    /// Memories **visible to `user_id`** in a workspace, most-recent first: all
    /// `Workspace`-scoped ones plus the acting user's own `User`-scoped ones.
    /// With `user_id = None` (an agent run with no member), only workspace-scoped
    /// memories are visible — a member's private memories are never leaked (§22).
    pub async fn list_visible(
        &self,
        workspace_id: WorkspaceId,
        user_id: Option<UserId>,
        limit: i64,
    ) -> Result<Vec<Memory>> {
        let rows: Vec<MemoryRow> = sqlx::query_as(&format!(
            "SELECT {MEMORY_COLS} FROM memories
             WHERE workspace_id = $1
               AND (scope = 'workspace' OR ($2::uuid IS NOT NULL AND user_id = $2))
             ORDER BY created_at DESC, id ASC
             LIMIT $3"
        ))
        .bind(workspace_id.into_uuid())
        .bind(user_id.map(UserId::into_uuid))
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(Memory::try_from).collect()
    }

    /// Replace a memory's `text` (workspace-scoped), bumping `updated_at`.
    /// [`StoreError::NotFound`] if no such memory exists in the workspace.
    pub async fn update_text(
        &self,
        workspace_id: WorkspaceId,
        id: MemoryId,
        text: &str,
    ) -> Result<Memory> {
        let row: MemoryRow = sqlx::query_as(&format!(
            "UPDATE memories SET text = $3, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2
             RETURNING {MEMORY_COLS}"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(text)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Bump a memory's `updated_at` **without** changing its text — the "reaffirm"
    /// applied on a dedup hit (SOUL §22/§29). When a `remember` / auto-curate store
    /// turns out to duplicate this row we skip the insert but touch it, so recency
    /// signals the fact was seen again while the row's id/created_at/scope are kept.
    /// Returns the (unchanged-text) row. [`StoreError::NotFound`] if absent.
    pub async fn touch(&self, workspace_id: WorkspaceId, id: MemoryId) -> Result<Memory> {
        let row: MemoryRow = sqlx::query_as(&format!(
            "UPDATE memories SET updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2
             RETURNING {MEMORY_COLS}"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Delete a memory, workspace-scoped. [`StoreError::NotFound`] if absent.
    pub async fn delete(&self, workspace_id: WorkspaceId, id: MemoryId) -> Result<()> {
        let res = sqlx::query("DELETE FROM memories WHERE id = $1 AND workspace_id = $2")
            .bind(id.into_uuid())
            .bind(workspace_id.into_uuid())
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
// Profiles (personalization, SOUL §22)
// ===========================================================================

/// The full set of `profiles` columns, in row order (for `SELECT`/`RETURNING`).
const PROFILE_COLS: &str = "workspace_id, user_id, fields, created_at, updated_at";

/// CRUD for the `profiles` table — the per-user structured record injected into
/// the chat system prompt every turn (SOUL §22). Keyed on `(workspace_id,
/// user_id)`.
#[derive(Clone, Debug)]
pub struct ProfileRepo {
    pool: PgPool,
}

impl ProfileRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The profile for `user_id` in `workspace_id`, or an **empty** profile (no
    /// fields) when none has been created yet — so callers always get a usable
    /// `Profile` without a `NotFound` branch.
    pub async fn get(&self, workspace_id: WorkspaceId, user_id: UserId) -> Result<Profile> {
        let row: Option<ProfileRow> = sqlx::query_as(&format!(
            "SELECT {PROFILE_COLS} FROM profiles WHERE workspace_id = $1 AND user_id = $2"
        ))
        .bind(workspace_id.into_uuid())
        .bind(user_id.into_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        Ok(match row {
            Some(r) => r.into(),
            None => Profile {
                workspace_id,
                user_id,
                fields: Map::new(),
            },
        })
    }

    /// Merge `fields` into the user's profile (top-level keys, the incoming value
    /// wins), creating the row on first write, and return the merged profile.
    /// Uses Postgres JSONB `||` so existing keys not in `fields` are preserved.
    /// An empty `fields` is a no-op upsert that just returns the current profile.
    pub async fn merge(
        &self,
        workspace_id: WorkspaceId,
        user_id: UserId,
        fields: &Map,
    ) -> Result<Profile> {
        let row: ProfileRow = sqlx::query_as(&format!(
            "INSERT INTO profiles (workspace_id, user_id, fields)
             VALUES ($1, $2, $3)
             ON CONFLICT (workspace_id, user_id)
             DO UPDATE SET fields = profiles.fields || EXCLUDED.fields,
                           updated_at = CURRENT_TIMESTAMP
             RETURNING {PROFILE_COLS}"
        ))
        .bind(workspace_id.into_uuid())
        .bind(user_id.into_uuid())
        .bind(Json(fields))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }
}

/// The full set of `llm_settings` columns, in row order (for `SELECT`/`RETURNING`).
const LLM_SETTINGS_COLS: &str = "workspace_id, user_id, chat_model, speech_model, \
     speech_voice, transcription_model, voice_input_speed, ocr_model, image_input_models, \
     created_at, updated_at";

/// CRUD for the `llm_settings` table — a per-user override of the `[llm]` config
/// model/voice defaults (SOUL §7/§13). Keyed on `(workspace_id, user_id)`; each
/// unset field falls back to the boot-time config default at use.
#[derive(Clone, Debug)]
pub struct LlmSettingsRepo {
    pool: PgPool,
}

impl LlmSettingsRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The settings for `user_id` in `workspace_id`, or a default record (model
    /// fields unset, microphone speed 1.5×) when none exists — so callers always
    /// get a usable value without a `NotFound` branch.
    pub async fn get(&self, workspace_id: WorkspaceId, user_id: UserId) -> Result<LlmSettings> {
        let row: Option<LlmSettingsRow> = sqlx::query_as(&format!(
            "SELECT {LLM_SETTINGS_COLS} FROM llm_settings WHERE workspace_id = $1 AND user_id = $2"
        ))
        .bind(workspace_id.into_uuid())
        .bind(user_id.into_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        Ok(match row {
            Some(r) => r.into(),
            None => LlmSettings {
                workspace_id,
                user_id,
                chat_model: None,
                speech_model: None,
                speech_voice: None,
                transcription_model: None,
                voice_input_speed: catalerum_core::model::default_voice_input_speed(),
                ocr_model: None,
                image_input_models: Vec::new(),
            },
        })
    }

    /// Replace the user's model/voice selections (a full upsert), creating the row
    /// on first write, and return the stored record. A `None` field **clears** that
    /// selection — the effective value then falls back to the `[llm]` config
    /// default — so a blank choice from the UI is sent as `None`.
    // One arg per selection column; bundling them into a struct would only move
    // the list one call up (the settings route is the sole real caller).
    #[allow(clippy::too_many_arguments)]
    pub async fn set(
        &self,
        workspace_id: WorkspaceId,
        user_id: UserId,
        chat_model: Option<&str>,
        speech_model: Option<&str>,
        speech_voice: Option<&str>,
        transcription_model: Option<&str>,
        voice_input_speed: f32,
        ocr_model: Option<&str>,
    ) -> Result<LlmSettings> {
        let row: LlmSettingsRow = sqlx::query_as(&format!(
            "INSERT INTO llm_settings
                 (workspace_id, user_id, chat_model, speech_model, speech_voice, transcription_model,
                  voice_input_speed, ocr_model)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (workspace_id, user_id)
             DO UPDATE SET chat_model = EXCLUDED.chat_model,
                           speech_model = EXCLUDED.speech_model,
                           speech_voice = EXCLUDED.speech_voice,
                           transcription_model = EXCLUDED.transcription_model,
                           voice_input_speed = EXCLUDED.voice_input_speed,
                           ocr_model = EXCLUDED.ocr_model,
                           updated_at = CURRENT_TIMESTAMP
             RETURNING {LLM_SETTINGS_COLS}"
        ))
        .bind(workspace_id.into_uuid())
        .bind(user_id.into_uuid())
        .bind(chat_model)
        .bind(speech_model)
        .bind(speech_voice)
        .bind(transcription_model)
        .bind(voice_input_speed)
        .bind(ocr_model)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Replace the user's **force-image-input** model list (SOUL §7/§9) — a
    /// column-scoped upsert that never disturbs the model/voice selections (and
    /// [`set`](Self::set) never disturbs this list), so the two writers coexist.
    /// Creates the row on first write; returns the stored record.
    pub async fn set_image_input_models(
        &self,
        workspace_id: WorkspaceId,
        user_id: UserId,
        models: &[String],
    ) -> Result<LlmSettings> {
        let row: LlmSettingsRow = sqlx::query_as(&format!(
            "INSERT INTO llm_settings (workspace_id, user_id, image_input_models)
             VALUES ($1, $2, $3)
             ON CONFLICT (workspace_id, user_id)
             DO UPDATE SET image_input_models = EXCLUDED.image_input_models,
                           updated_at = CURRENT_TIMESTAMP
             RETURNING {LLM_SETTINGS_COLS}"
        ))
        .bind(workspace_id.into_uuid())
        .bind(user_id.into_uuid())
        .bind(sqlx::types::Json(models))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }
}

/// The full set of `search_settings` columns, in row order (for `SELECT`/`RETURNING`).
const SEARCH_SETTINGS_COLS: &str =
    "workspace_id, user_id, default_provider, created_at, updated_at";

/// CRUD for the `search_settings` table — a per-user override of the `[search]`
/// default provider (SOUL §7/§13). Keyed on `(workspace_id, user_id)`; an unset
/// `default_provider` falls back to the boot-time `[search].backend` at use.
/// Holds nothing secret — provider API keys live only in config (SOUL §13).
#[derive(Clone, Debug)]
pub struct SearchSettingsRepo {
    pool: PgPool,
}

impl SearchSettingsRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The settings for `user_id` in `workspace_id`, or an **empty** record
    /// (provider unset) when none exists — so callers always get a usable value
    /// without a `NotFound` branch (an unset choice then falls back to config).
    pub async fn get(&self, workspace_id: WorkspaceId, user_id: UserId) -> Result<SearchSettings> {
        let row: Option<SearchSettingsRow> = sqlx::query_as(&format!(
            "SELECT {SEARCH_SETTINGS_COLS} FROM search_settings WHERE workspace_id = $1 AND user_id = $2"
        ))
        .bind(workspace_id.into_uuid())
        .bind(user_id.into_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        Ok(match row {
            Some(r) => r.into(),
            None => SearchSettings {
                workspace_id,
                user_id,
                default_provider: None,
            },
        })
    }

    /// Replace the user's default-provider selection (a full upsert), creating the
    /// row on first write, and return the stored record. A `None` value **clears**
    /// the selection — the effective default then falls back to `[search].backend`
    /// — so a blank choice from the UI is sent as `None`.
    pub async fn set(
        &self,
        workspace_id: WorkspaceId,
        user_id: UserId,
        default_provider: Option<&str>,
    ) -> Result<SearchSettings> {
        let row: SearchSettingsRow = sqlx::query_as(&format!(
            "INSERT INTO search_settings (workspace_id, user_id, default_provider)
             VALUES ($1, $2, $3)
             ON CONFLICT (workspace_id, user_id)
             DO UPDATE SET default_provider = EXCLUDED.default_provider,
                           updated_at = CURRENT_TIMESTAMP
             RETURNING {SEARCH_SETTINGS_COLS}"
        ))
        .bind(workspace_id.into_uuid())
        .bind(user_id.into_uuid())
        .bind(default_provider)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }
}

/// The full set of `storage_settings` columns, in row order (for `SELECT`/`RETURNING`).
const STORAGE_SETTINGS_COLS: &str = "workspace_id, user_id, default_store, created_at, updated_at";

/// CRUD for the `storage_settings` table — a per-user override of the default
/// files store (SOUL §7/§9/§13). Keyed on `(workspace_id, user_id)`; an unset
/// `default_store` falls back to the boot-time `[storage]` config default at use.
/// Holds nothing secret — backend credentials live only in config / a
/// `Connection`'s credential ref (SOUL §13).
#[derive(Clone, Debug)]
pub struct StorageSettingsRepo {
    pool: PgPool,
}

impl StorageSettingsRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The settings for `user_id` in `workspace_id`, or an **empty** record
    /// (store unset) when none exists — so callers always get a usable value
    /// without a `NotFound` branch (an unset choice then falls back to config).
    pub async fn get(&self, workspace_id: WorkspaceId, user_id: UserId) -> Result<StorageSettings> {
        let row: Option<StorageSettingsRow> = sqlx::query_as(&format!(
            "SELECT {STORAGE_SETTINGS_COLS} FROM storage_settings WHERE workspace_id = $1 AND user_id = $2"
        ))
        .bind(workspace_id.into_uuid())
        .bind(user_id.into_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        Ok(match row {
            Some(r) => r.into(),
            None => StorageSettings {
                workspace_id,
                user_id,
                default_store: None,
            },
        })
    }

    /// Replace the user's default-store selection (a full upsert), creating the
    /// row on first write, and return the stored record. A `None` value **clears**
    /// the selection — the effective default then falls back to the `[storage]`
    /// config default — so a blank choice from the UI is sent as `None`.
    pub async fn set(
        &self,
        workspace_id: WorkspaceId,
        user_id: UserId,
        default_store: Option<&str>,
    ) -> Result<StorageSettings> {
        let row: StorageSettingsRow = sqlx::query_as(&format!(
            "INSERT INTO storage_settings (workspace_id, user_id, default_store)
             VALUES ($1, $2, $3)
             ON CONFLICT (workspace_id, user_id)
             DO UPDATE SET default_store = EXCLUDED.default_store,
                           updated_at = CURRENT_TIMESTAMP
             RETURNING {STORAGE_SETTINGS_COLS}"
        ))
        .bind(workspace_id.into_uuid())
        .bind(user_id.into_uuid())
        .bind(default_store)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }
}

// ===========================================================================
// Per-App durable key/value store (SOUL §12/§29)
// ===========================================================================

const APP_DATA_COLS: &str = "app, key, value, created_at, updated_at";

/// Max serialized bytes of a single stored value. Mirrors the `initial_state`
/// cap philosophy (SOUL §12): a value is a small JSON document (a saved layout, a
/// per-user tracker), not a blob — files/external Postgres are the home for bulk.
pub const MAX_APP_DATA_VALUE_BYTES: usize = 64 * 1024;

/// Max keys one App (one `(workspace, app)` namespace) may hold. Bounds the store
/// so an App cannot grow unbounded rows; a data model that outgrows this belongs
/// in a first-class store (SOUL §12).
pub const MAX_APP_DATA_KEYS_PER_APP: i64 = 1_000;

/// One entry in the per-App key/value store: `(app, key) → value` plus timestamps.
#[derive(Clone, Debug)]
pub struct AppDataEntry {
    /// The App namespace this entry lives under (a UI id on the handler path).
    pub app: String,
    pub key: String,
    pub value: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<AppDataRow> for AppDataEntry {
    fn from(r: AppDataRow) -> Self {
        AppDataEntry {
            app: r.app,
            key: r.key,
            value: r.value.0,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

/// The `app_data` table (SOUL §12/§29): a workspace-scoped `(app, key) → JSONB`
/// map the emerged-App tools (`app_data_get`/`set`/`list`/`delete`) are thin
/// clients of. Every method is workspace-filtered; the `app` namespace is chosen
/// by the *caller* of the tool (forced to the firing UI's id on the handler path,
/// so cross-App reads are impossible there — see the migration + the tools).
#[derive(Clone, Debug)]
pub struct AppDataRepo {
    pool: PgPool,
}

impl AppDataRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Fetch one entry by `(app, key)`, or `None` when unset. `app`/`key` are
    /// trimmed and must be non-empty.
    pub async fn get(
        &self,
        workspace_id: WorkspaceId,
        app: &str,
        key: &str,
    ) -> Result<Option<AppDataEntry>> {
        let (app, key) = Self::validate_key(app, key)?;
        let row: Option<AppDataRow> = sqlx::query_as(&format!(
            "SELECT {APP_DATA_COLS} FROM app_data
             WHERE workspace_id = $1 AND app = $2 AND key = $3"
        ))
        .bind(workspace_id.into_uuid())
        .bind(app)
        .bind(key)
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.map(Into::into))
    }

    /// Upsert `value` at `(app, key)`, returning the stored entry. Enforces the
    /// per-value byte cap ([`MAX_APP_DATA_VALUE_BYTES`]) and the per-App key cap
    /// ([`MAX_APP_DATA_KEYS_PER_APP`], only when introducing a *new* key — an
    /// update of an existing key never grows the row count). Idempotent on
    /// `(workspace, app, key)`.
    pub async fn set(
        &self,
        workspace_id: WorkspaceId,
        app: &str,
        key: &str,
        value: &serde_json::Value,
    ) -> Result<AppDataEntry> {
        let (app, key) = Self::validate_key(app, key)?;
        // Byte-bound the value (serialize once; the bind below re-serializes, but
        // this is the authoritative size check the caller sees).
        let bytes = serde_json::to_vec(value).map_err(StoreError::decode)?;
        if bytes.len() > MAX_APP_DATA_VALUE_BYTES {
            return Err(StoreError::invalid(format!(
                "value is {} bytes (max {MAX_APP_DATA_VALUE_BYTES}); \
                 store large data in a file or external database instead",
                bytes.len()
            )));
        }
        // Row cap: reject a *new* key once the namespace is full (an update to an
        // existing key is always allowed). Best-effort check-then-insert — the cap
        // is a guardrail, not a hard transactional invariant.
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM app_data
                 WHERE workspace_id = $1 AND app = $2 AND key = $3)",
        )
        .bind(workspace_id.into_uuid())
        .bind(&app)
        .bind(&key)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        if !exists {
            let count = self.count(workspace_id, &app).await?;
            if count >= MAX_APP_DATA_KEYS_PER_APP {
                return Err(StoreError::invalid(format!(
                    "app `{app}` already holds {count} keys (max {MAX_APP_DATA_KEYS_PER_APP})"
                )));
            }
        }
        let row: AppDataRow = sqlx::query_as(&format!(
            "INSERT INTO app_data (workspace_id, app, key, value)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (workspace_id, app, key)
             DO UPDATE SET value = EXCLUDED.value, updated_at = CURRENT_TIMESTAMP
             RETURNING {APP_DATA_COLS}"
        ))
        .bind(workspace_id.into_uuid())
        .bind(app)
        .bind(key)
        .bind(Json(value))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Every entry in one `(workspace, app)` namespace, key-ordered, bounded to
    /// `limit` (floored at 1). `app` is trimmed and must be non-empty.
    pub async fn list(
        &self,
        workspace_id: WorkspaceId,
        app: &str,
        limit: i64,
    ) -> Result<Vec<AppDataEntry>> {
        let app = app.trim();
        if app.is_empty() {
            return Err(StoreError::invalid("`app` must not be empty"));
        }
        let rows: Vec<AppDataRow> = sqlx::query_as(&format!(
            "SELECT {APP_DATA_COLS} FROM app_data
             WHERE workspace_id = $1 AND app = $2
             ORDER BY key ASC
             LIMIT $3"
        ))
        .bind(workspace_id.into_uuid())
        .bind(app)
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Number of keys in one `(workspace, app)` namespace (for the row cap + a
    /// `list` count).
    pub async fn count(&self, workspace_id: WorkspaceId, app: &str) -> Result<i64> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM app_data WHERE workspace_id = $1 AND app = $2",
        )
        .bind(workspace_id.into_uuid())
        .bind(app.trim())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(n)
    }

    /// Delete one entry by `(app, key)`. Returns whether a row was removed
    /// (idempotent — deleting an absent key is a no-op returning `false`).
    pub async fn delete(&self, workspace_id: WorkspaceId, app: &str, key: &str) -> Result<bool> {
        let (app, key) = Self::validate_key(app, key)?;
        let done =
            sqlx::query("DELETE FROM app_data WHERE workspace_id = $1 AND app = $2 AND key = $3")
                .bind(workspace_id.into_uuid())
                .bind(app)
                .bind(key)
                .execute(&self.pool)
                .await
                .map_err(map)?;
        Ok(done.rows_affected() > 0)
    }

    /// Trim + non-empty-check an `(app, key)` pair, returning the cleaned owned
    /// pair or [`StoreError::Invalid`].
    fn validate_key(app: &str, key: &str) -> Result<(String, String)> {
        let app = app.trim();
        let key = key.trim();
        if app.is_empty() {
            return Err(StoreError::invalid("`app` must not be empty"));
        }
        if key.is_empty() {
            return Err(StoreError::invalid("`key` must not be empty"));
        }
        Ok((app.to_string(), key.to_string()))
    }
}

// ===========================================================================
// Tasks & Kanban board (SOUL §24)
// ===========================================================================

const BOARD_COLS: &str = "id, workspace_id, name, created_at, updated_at";
const COLUMN_COLS: &str = "id, board_id, name, ordinal";
const TASK_COLS: &str = "id, workspace_id, board_id, column_id, title, body_md, \
    assignee_kind, assignee_id, ordinal, status, created_at, updated_at";

/// The default Kanban columns when a board is created without an explicit set
/// (SOUL §24).
pub const DEFAULT_COLUMNS: &[&str] = &["Backlog", "To-do", "Doing", "Done"];

/// CRUD for `boards` + `board_columns` (SOUL §24). A board owns an ordered set of
/// columns; tasks live in columns ([`TaskRepo`]).
#[derive(Clone, Debug)]
pub struct BoardRepo {
    pool: PgPool,
}

impl BoardRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create a board with its columns (in order). An empty `columns` uses
    /// [`DEFAULT_COLUMNS`]. Returns the assembled [`Board`].
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        name: &str,
        columns: &[&str],
    ) -> Result<Board> {
        let names: Vec<&str> = if columns.is_empty() {
            DEFAULT_COLUMNS.to_vec()
        } else {
            columns.to_vec()
        };
        let mut tx = self.pool.begin().await.map_err(map)?;
        let board: BoardRow = sqlx::query_as(&format!(
            "INSERT INTO boards (id, workspace_id, name) VALUES ($1, $2, $3)
             RETURNING {BOARD_COLS}"
        ))
        .bind(BoardId::new().into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(name)
        .fetch_one(&mut *tx)
        .await
        .map_err(map)?;

        let mut column_rows = Vec::with_capacity(names.len());
        for (ordinal, col_name) in names.iter().enumerate() {
            let col: ColumnRow = sqlx::query_as(&format!(
                "INSERT INTO board_columns (id, workspace_id, board_id, name, ordinal)
                 VALUES ($1, $2, $3, $4, $5)
                 RETURNING {COLUMN_COLS}"
            ))
            .bind(ColumnId::new().into_uuid())
            .bind(workspace_id.into_uuid())
            .bind(board.id)
            .bind(*col_name)
            .bind(ordinal as i32)
            .fetch_one(&mut *tx)
            .await
            .map_err(map)?;
            column_rows.push(col);
        }
        tx.commit().await.map_err(map)?;
        Ok(board_from_parts(board, column_rows))
    }

    /// Fetch a board with its columns (ordered), workspace-scoped.
    pub async fn get(&self, workspace_id: WorkspaceId, id: BoardId) -> Result<Board> {
        let board: BoardRow = sqlx::query_as(&format!(
            "SELECT {BOARD_COLS} FROM boards WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        let columns = self.columns_of(id).await?;
        Ok(board_from_parts(board, columns))
    }

    /// List a workspace's boards (with columns), by name.
    pub async fn list_by_workspace(&self, workspace_id: WorkspaceId) -> Result<Vec<Board>> {
        let boards: Vec<BoardRow> = sqlx::query_as(&format!(
            "SELECT {BOARD_COLS} FROM boards WHERE workspace_id = $1 ORDER BY name ASC, id ASC"
        ))
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        let mut out = Vec::with_capacity(boards.len());
        for b in boards {
            let columns = self.columns_of(BoardId::from_uuid(b.id)).await?;
            out.push(board_from_parts(b, columns));
        }
        Ok(out)
    }

    /// Append a column to a board (at the next ordinal), workspace-scoped.
    /// Returns the updated board. `NotFound` if no such board.
    pub async fn add_column(
        &self,
        workspace_id: WorkspaceId,
        board_id: BoardId,
        name: &str,
    ) -> Result<Board> {
        // Confirm the board exists in this workspace first (NotFound, not an FK error).
        let board: BoardRow = sqlx::query_as(&format!(
            "SELECT {BOARD_COLS} FROM boards WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(board_id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        sqlx::query(
            "INSERT INTO board_columns (id, workspace_id, board_id, name, ordinal)
             VALUES ($1, $2, $3, $4,
                     COALESCE((SELECT max(ordinal) + 1 FROM board_columns WHERE board_id = $3), 0))",
        )
        .bind(ColumnId::new().into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(board_id.into_uuid())
        .bind(name)
        .execute(&self.pool)
        .await
        .map_err(map)?;
        let columns = self.columns_of(board_id).await?;
        Ok(board_from_parts(board, columns))
    }

    /// Rename a column (workspace-scoped). Returns its updated board.
    /// `NotFound` if no such column.
    pub async fn rename_column(
        &self,
        workspace_id: WorkspaceId,
        id: ColumnId,
        name: &str,
    ) -> Result<Board> {
        let col: ColumnRow = sqlx::query_as(&format!(
            "UPDATE board_columns SET name = $3 WHERE id = $1 AND workspace_id = $2
             RETURNING {COLUMN_COLS}"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(name)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        self.get(workspace_id, BoardId::from_uuid(col.board_id))
            .await
    }

    /// Delete an **empty** column (workspace-scoped). Returns the updated board.
    /// `Invalid` when tasks still sit in it (move or delete them first) or when
    /// it is the board's only column; `NotFound` if no such column.
    pub async fn delete_column(&self, workspace_id: WorkspaceId, id: ColumnId) -> Result<Board> {
        let mut tx = self.pool.begin().await.map_err(map)?;
        #[cfg(not(feature = "sqlite"))]
        let row_lock = "FOR UPDATE";
        #[cfg(feature = "sqlite")]
        let row_lock = "";
        let col: ColumnRow = sqlx::query_as(&format!(
            "SELECT {COLUMN_COLS} FROM board_columns
             WHERE id = $1 AND workspace_id = $2 {row_lock}"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&mut *tx)
        .await
        .map_err(map)?;
        let (tasks,): (i64,) = sqlx::query_as("SELECT count(*) FROM tasks WHERE column_id = $1")
            .bind(id.into_uuid())
            .fetch_one(&mut *tx)
            .await
            .map_err(map)?;
        if tasks > 0 {
            return Err(StoreError::Invalid(format!(
                "column `{}` still has {tasks} task(s) — move or delete them first",
                col.name
            )));
        }
        let (siblings,): (i64,) =
            sqlx::query_as("SELECT count(*) FROM board_columns WHERE board_id = $1")
                .bind(col.board_id)
                .fetch_one(&mut *tx)
                .await
                .map_err(map)?;
        if siblings <= 1 {
            return Err(StoreError::Invalid(
                "a board needs at least one column".to_string(),
            ));
        }
        sqlx::query("DELETE FROM board_columns WHERE id = $1")
            .bind(id.into_uuid())
            .execute(&mut *tx)
            .await
            .map_err(map)?;
        tx.commit().await.map_err(map)?;
        self.get(workspace_id, BoardId::from_uuid(col.board_id))
            .await
    }

    /// Rename a board (workspace-scoped). Returns the updated board with its
    /// columns. `NotFound` if no such board.
    pub async fn rename(
        &self,
        workspace_id: WorkspaceId,
        id: BoardId,
        name: &str,
    ) -> Result<Board> {
        let board: BoardRow = sqlx::query_as(&format!(
            "UPDATE boards SET name = $3 WHERE id = $1 AND workspace_id = $2 RETURNING {BOARD_COLS}"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(name)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        let columns = self.columns_of(id).await?;
        Ok(board_from_parts(board, columns))
    }

    /// Delete a board by id (workspace-scoped); its columns + tasks cascade via
    /// the `0008` `ON DELETE CASCADE` FKs. `NotFound` if no such board.
    pub async fn delete(&self, workspace_id: WorkspaceId, id: BoardId) -> Result<()> {
        let res = sqlx::query("DELETE FROM boards WHERE id = $1 AND workspace_id = $2")
            .bind(id.into_uuid())
            .bind(workspace_id.into_uuid())
            .execute(&self.pool)
            .await
            .map_err(map)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    async fn columns_of(&self, board_id: BoardId) -> Result<Vec<ColumnRow>> {
        sqlx::query_as(&format!(
            "SELECT {COLUMN_COLS} FROM board_columns WHERE board_id = $1 ORDER BY ordinal ASC"
        ))
        .bind(board_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)
    }
}

/// CRUD for the `tasks` table (SOUL §24). Tasks are created in a column and
/// worked one-by-one: pull `next`, do the work, `complete`.
#[derive(Clone, Debug)]
pub struct TaskRepo {
    pool: PgPool,
}

impl TaskRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create a task at the end of `column_id` (next `ordinal`), status `open`.
    /// The column is verified to belong to `board_id` in `workspace_id` first.
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        board_id: BoardId,
        column_id: ColumnId,
        title: &str,
        body_md: &str,
        assignee: Option<Author>,
    ) -> Result<Task> {
        self.verify_column(workspace_id, board_id, column_id)
            .await?;
        let (assignee_kind, assignee_id) = match assignee {
            Some(a) => {
                let (k, i) = author_to_parts(a);
                (Some(k), Some(i))
            }
            None => (None, None),
        };
        let row: TaskRow = sqlx::query_as(&format!(
            "INSERT INTO tasks
                 (id, workspace_id, board_id, column_id, title, body_md, assignee_kind,
                  assignee_id, ordinal, status)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8,
                     COALESCE((SELECT max(ordinal) + 1 FROM tasks WHERE column_id = $4), 0),
                     'open')
             RETURNING {TASK_COLS}"
        ))
        .bind(TaskId::new().into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(board_id.into_uuid())
        .bind(column_id.into_uuid())
        .bind(title)
        .bind(body_md)
        .bind(assignee_kind)
        .bind(assignee_id)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Fetch a task by id, workspace-scoped. [`StoreError::NotFound`] if absent.
    pub async fn get(&self, workspace_id: WorkspaceId, id: TaskId) -> Result<Task> {
        let row: TaskRow = sqlx::query_as(&format!(
            "SELECT {TASK_COLS} FROM tasks WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Search tasks by the **content** of their title or body (SOUL §24/§6.1) —
    /// `query` matched as a **literal case-insensitive substring** (`strpos`, not
    /// `LIKE`, so a user's `%`/`_` are literal), workspace-scoped, most-recently-
    /// updated first, `LIMIT`-bounded (floored at 1). A blank query returns nothing
    /// (no "match everything"). The literal complement to `list_by_workspace`'s
    /// status/board filtering — drives the `search_tasks` agent tool.
    pub async fn search_in_workspace(
        &self,
        workspace_id: WorkspaceId,
        query: &str,
        limit: i64,
    ) -> Result<Vec<Task>> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<TaskRow> = sqlx::query_as(&format!(
            "SELECT {TASK_COLS} FROM tasks
             WHERE workspace_id = $1
               AND (strpos(lower(title), lower($2)) > 0 OR strpos(lower(body_md), lower($2)) > 0)
             ORDER BY updated_at DESC, id DESC
             LIMIT $3"
        ))
        .bind(workspace_id.into_uuid())
        .bind(query)
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(Task::try_from).collect()
    }

    /// Move a task to `to_column`, workspace-scoped. The target column must be
    /// in the same board. `position` is the task's final 0-based index among the
    /// column's tasks (clamped; `None` = the end) — so `Some(0)` puts it on top,
    /// and a same-column move with a `position` is a within-column reorder.
    /// Returns the updated task.
    pub async fn move_to_column(
        &self,
        workspace_id: WorkspaceId,
        id: TaskId,
        to_column: ColumnId,
        position: Option<i32>,
    ) -> Result<Task> {
        let row: TaskRow = sqlx::query_as(&format!(
            "UPDATE tasks t SET
                 column_id = $3,
                 ordinal = COALESCE((SELECT max(ordinal) + 1 FROM tasks WHERE column_id = $3), 0),
                 updated_at = CURRENT_TIMESTAMP
             WHERE t.id = $1 AND t.workspace_id = $2
               AND EXISTS (SELECT 1 FROM board_columns c
                           WHERE c.id = $3 AND c.board_id = t.board_id)
             RETURNING {TASK_COLS}"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(to_column.into_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?
        .ok_or(StoreError::NotFound)?;
        match position {
            None => row.try_into(),
            Some(p) => self.place_at(workspace_id, id, to_column, p).await,
        }
    }

    /// Renumber `column_id` densely (0..n) with task `id` at the clamped
    /// `position`. The task must already sit in the column (`move_to_column`
    /// puts it there first). Row-locked so two concurrent placements can't
    /// interleave their renumbering.
    async fn place_at(
        &self,
        workspace_id: WorkspaceId,
        id: TaskId,
        column_id: ColumnId,
        position: i32,
    ) -> Result<Task> {
        let mut tx = self.pool.begin().await.map_err(map)?;
        #[cfg(not(feature = "sqlite"))]
        let row_lock = "FOR UPDATE";
        #[cfg(feature = "sqlite")]
        let row_lock = "";
        let siblings: Vec<(Uuid,)> = sqlx::query_as(&format!(
            "SELECT id FROM tasks
             WHERE workspace_id = $1 AND column_id = $2 AND id <> $3
             ORDER BY ordinal ASC, id ASC
             {row_lock}"
        ))
        .bind(workspace_id.into_uuid())
        .bind(column_id.into_uuid())
        .bind(id.into_uuid())
        .fetch_all(&mut *tx)
        .await
        .map_err(map)?;
        let mut ordered: Vec<Uuid> = siblings.into_iter().map(|(u,)| u).collect();
        let pos = usize::try_from(position.max(0))
            .unwrap_or(0)
            .min(ordered.len());
        ordered.insert(pos, id.into_uuid());
        for (ordinal, task) in ordered.iter().enumerate() {
            sqlx::query("UPDATE tasks SET ordinal = $3 WHERE id = $1 AND workspace_id = $2")
                .bind(task)
                .bind(workspace_id.into_uuid())
                .bind(ordinal as i32)
                .execute(&mut *tx)
                .await
                .map_err(map)?;
        }
        tx.commit().await.map_err(map)?;
        self.get(workspace_id, id).await
    }

    /// The next task to work in a column: the lowest-`ordinal` task not yet
    /// `done` (SOUL §24). `None` if the column is empty/all done.
    pub async fn next_in_column(
        &self,
        workspace_id: WorkspaceId,
        column_id: ColumnId,
    ) -> Result<Option<Task>> {
        let row: Option<TaskRow> = sqlx::query_as(&format!(
            "SELECT {TASK_COLS} FROM tasks
             WHERE workspace_id = $1 AND column_id = $2 AND status <> 'done'
             ORDER BY ordinal ASC, id ASC
             LIMIT 1"
        ))
        .bind(workspace_id.into_uuid())
        .bind(column_id.into_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        row.map(Task::try_from).transpose()
    }

    /// Set a task's `status` (workspace-scoped). [`StoreError::NotFound`] if
    /// absent. Used by `complete` (status = `done`).
    pub async fn set_status(
        &self,
        workspace_id: WorkspaceId,
        id: TaskId,
        status: TaskStatus,
    ) -> Result<Task> {
        let row: TaskRow = sqlx::query_as(&format!(
            "UPDATE tasks SET status = $3, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2
             RETURNING {TASK_COLS}"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(task_status_to_text(status))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Update a task's title + markdown body (workspace-scoped). `NotFound` if no
    /// such task. Status / column / ordinal are untouched — this is the card-edit
    /// counterpart to `set_status` / `move_to_column`.
    pub async fn update(
        &self,
        workspace_id: WorkspaceId,
        id: TaskId,
        title: &str,
        body_md: &str,
    ) -> Result<Task> {
        let row: TaskRow = sqlx::query_as(&format!(
            "UPDATE tasks SET title = $3, body_md = $4, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2
             RETURNING {TASK_COLS}"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(title)
        .bind(body_md)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Delete a task by id (workspace-scoped). `NotFound` if no such task. No row
    /// references `tasks(id)`, so this is a plain single-row delete (no cascade).
    pub async fn delete(&self, workspace_id: WorkspaceId, id: TaskId) -> Result<()> {
        let res = sqlx::query("DELETE FROM tasks WHERE id = $1 AND workspace_id = $2")
            .bind(id.into_uuid())
            .bind(workspace_id.into_uuid())
            .execute(&self.pool)
            .await
            .map_err(map)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// List a column's tasks in order, workspace-scoped.
    pub async fn list_by_column(
        &self,
        workspace_id: WorkspaceId,
        column_id: ColumnId,
    ) -> Result<Vec<Task>> {
        let rows: Vec<TaskRow> = sqlx::query_as(&format!(
            "SELECT {TASK_COLS} FROM tasks
             WHERE workspace_id = $1 AND column_id = $2
             ORDER BY ordinal ASC, id ASC"
        ))
        .bind(workspace_id.into_uuid())
        .bind(column_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(Task::try_from).collect()
    }

    /// List **all** of a workspace's tasks, board-grouped then in-column order —
    /// the workspace-wide read backing `query_structured`'s task lookups (§6.5/§24).
    /// Ordered by `board_id`, then column `ordinal`, then task `ordinal` for a
    /// stable, board-by-board, top-of-column-first sequence.
    pub async fn list_by_workspace(&self, workspace_id: WorkspaceId) -> Result<Vec<Task>> {
        let rows: Vec<TaskRow> = sqlx::query_as(&format!(
            "SELECT {TASK_COLS} FROM tasks t
             WHERE t.workspace_id = $1
             ORDER BY t.board_id ASC,
                      (SELECT c.ordinal FROM board_columns c WHERE c.id = t.column_id) ASC,
                      t.ordinal ASC, t.id ASC"
        ))
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(Task::try_from).collect()
    }

    /// Verify `column_id` belongs to `board_id` within `workspace_id`, or
    /// [`StoreError::NotFound`] — so a task can't be created across the tenancy
    /// boundary or in a foreign board (§18).
    async fn verify_column(
        &self,
        workspace_id: WorkspaceId,
        board_id: BoardId,
        column_id: ColumnId,
    ) -> Result<()> {
        let found: Option<(Uuid,)> = sqlx::query_as(
            "SELECT id FROM board_columns
             WHERE id = $1 AND board_id = $2 AND workspace_id = $3",
        )
        .bind(column_id.into_uuid())
        .bind(board_id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        if found.is_none() {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }
}

// ===========================================================================
// Skills (SOUL §23)
// ===========================================================================

const SKILL_COLS: &str = "id, workspace_id, name, description, instructions_md, tools, code, \
     advertised, created_at, updated_at";

/// The fields to create or upsert a [`Skill`] (SOUL §23).
#[derive(Clone, Debug)]
pub struct NewSkill {
    /// Unique (per workspace) skill name — how it is invoked.
    pub name: String,
    /// One-line description.
    pub description: String,
    /// Markdown runbook / instructions.
    pub instructions_md: String,
    /// Tool names the skill may use (a subset of the registry).
    pub tools: Vec<String>,
    /// Optional executable code (run via the Executor §20).
    pub code: Option<Code>,
    /// Whether the skill's name + description are advertised in the chat system
    /// prompt ("visible to agent", SOUL §23). `true` by default.
    pub advertised: bool,
}

/// CRUD for the `skills` table (SOUL §23). Skills are named, workspace-scoped,
/// and invoked by name via the `use_skill` tool.
#[derive(Clone, Debug)]
pub struct SkillRepo {
    pool: PgPool,
}

impl SkillRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create a skill. Errors with [`StoreError::Conflict`] if `name` already
    /// exists in the workspace (the `(workspace_id, name)` unique key).
    pub async fn create(&self, workspace_id: WorkspaceId, skill: &NewSkill) -> Result<Skill> {
        let row: SkillRow = sqlx::query_as(&format!(
            "INSERT INTO skills
                 (id, workspace_id, name, description, instructions_md, tools, code, advertised)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             RETURNING {SKILL_COLS}"
        ))
        .bind(SkillId::new().into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(&skill.name)
        .bind(&skill.description)
        .bind(&skill.instructions_md)
        .bind(Json(skill.tools.clone()))
        .bind(skill.code.as_ref().map(Json))
        .bind(skill.advertised)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Idempotently upsert a skill by `(workspace_id, name)` — create it, or
    /// replace its definition. Used to seed first-party skills (SOUL §23).
    pub async fn upsert_by_name(
        &self,
        workspace_id: WorkspaceId,
        skill: &NewSkill,
    ) -> Result<Skill> {
        let row: SkillRow = sqlx::query_as(&format!(
            "INSERT INTO skills
                 (id, workspace_id, name, description, instructions_md, tools, code, advertised)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (workspace_id, name) DO UPDATE SET
                 description = EXCLUDED.description,
                 instructions_md = EXCLUDED.instructions_md,
                 tools = EXCLUDED.tools,
                 code = EXCLUDED.code,
                 advertised = EXCLUDED.advertised,
                 updated_at = CURRENT_TIMESTAMP
             RETURNING {SKILL_COLS}"
        ))
        .bind(SkillId::new().into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(&skill.name)
        .bind(&skill.description)
        .bind(&skill.instructions_md)
        .bind(Json(skill.tools.clone()))
        .bind(skill.code.as_ref().map(Json))
        .bind(skill.advertised)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch a skill by id, workspace-scoped.
    pub async fn get(&self, workspace_id: WorkspaceId, id: SkillId) -> Result<Skill> {
        let row: SkillRow = sqlx::query_as(&format!(
            "SELECT {SKILL_COLS} FROM skills WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch a skill by name (how `use_skill` resolves it), or `None`.
    pub async fn get_by_name(
        &self,
        workspace_id: WorkspaceId,
        name: &str,
    ) -> Result<Option<Skill>> {
        let row: Option<SkillRow> = sqlx::query_as(&format!(
            "SELECT {SKILL_COLS} FROM skills WHERE workspace_id = $1 AND name = $2"
        ))
        .bind(workspace_id.into_uuid())
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.map(Skill::from))
    }

    /// Fetch multiple skills by name in **one** query (absent names omitted), so a
    /// caller resolving an agent's named skills avoids an N+1 of
    /// [`get_by_name`](Self::get_by_name).
    #[cfg(not(feature = "sqlite"))]
    pub async fn get_many_by_name(
        &self,
        workspace_id: WorkspaceId,
        names: &[String],
    ) -> Result<Vec<Skill>> {
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<SkillRow> = sqlx::query_as(&format!(
            "SELECT {SKILL_COLS} FROM skills WHERE workspace_id = $1 AND name = ANY($2)"
        ))
        .bind(workspace_id.into_uuid())
        .bind(names)
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Skill::from).collect())
    }

    #[cfg(feature = "sqlite")]
    pub async fn get_many_by_name(
        &self,
        workspace_id: WorkspaceId,
        names: &[String],
    ) -> Result<Vec<Skill>> {
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(format!(
            "SELECT {SKILL_COLS} FROM skills WHERE workspace_id = "
        ));
        query
            .push_bind(workspace_id.into_uuid())
            .push(" AND name IN (");
        let mut values = query.separated(", ");
        for name in names {
            values.push_bind(name);
        }
        values.push_unseparated(")");
        let rows: Vec<SkillRow> = query
            .build_query_as()
            .fetch_all(&self.pool)
            .await
            .map_err(map)?;
        Ok(rows.into_iter().map(Skill::from).collect())
    }

    /// `(name, description)` of every **advertised** skill in the workspace, by
    /// name — the lean read the chat system prompt's skill-advertising block
    /// makes every turn (SOUL §23), skipping the runbooks/code `SKILL_COLS`
    /// would drag along.
    pub async fn list_advertised(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<(String, String)>> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT name, description FROM skills
             WHERE workspace_id = $1 AND advertised
             ORDER BY name ASC, id ASC",
        )
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows)
    }

    /// List a workspace's skills, by name.
    pub async fn list_by_workspace(&self, workspace_id: WorkspaceId) -> Result<Vec<Skill>> {
        let rows: Vec<SkillRow> = sqlx::query_as(&format!(
            "SELECT {SKILL_COLS} FROM skills WHERE workspace_id = $1 ORDER BY name ASC, id ASC"
        ))
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Skill::from).collect())
    }

    /// Delete a skill, workspace-scoped. [`StoreError::NotFound`] if absent.
    pub async fn delete(&self, workspace_id: WorkspaceId, id: SkillId) -> Result<()> {
        let res = sqlx::query("DELETE FROM skills WHERE id = $1 AND workspace_id = $2")
            .bind(id.into_uuid())
            .bind(workspace_id.into_uuid())
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
// Agent profiles (SOUL §19/§25)
// ===========================================================================

const AGENT_PROFILE_COLS: &str = "id, workspace_id, name, model, system_prompt, tools, skills, \
     subagents, channels, grant_id, guard, created_at, updated_at";

/// The fields to create or upsert an [`AgentProfile`] (SOUL §19). The name lists
/// (`tools`/`skills`/`subagents`/`channels`) and the optional `grant_id` bound the
/// profile's authority and behaviour.
#[derive(Clone, Debug)]
pub struct NewAgentProfile {
    /// Unique (per workspace) profile name.
    pub name: String,
    /// Model id to run against; `None` uses the workspace default.
    pub model: Option<String>,
    /// System prompt; `None` uses the default agent system prompt.
    pub system_prompt: Option<String>,
    /// Tool names the profile may dispatch (subset of the registry); empty = all.
    pub tools: Vec<String>,
    /// Skill names whose runbooks seed the system prompt.
    pub skills: Vec<String>,
    /// Agent-profile names this profile may delegate to (subagents).
    pub subagents: Vec<String>,
    /// Channel names this profile listens on.
    pub channels: Vec<String>,
    /// The §19 grant that is this profile's authority (`None` = base Member).
    pub grant_id: Option<GrantId>,
    /// Optional per-profile tool guard (SOUL §19); `None` leaves the profile gated
    /// only by its capabilities.
    pub guard: Option<ToolGuard>,
}

/// CRUD for the `agent_profiles` table (SOUL §19/§25). Profiles are named,
/// workspace-scoped scoped-agent configurations; the API resolves them by name
/// and the channel router resolves the ones listening on a channel.
#[derive(Clone, Debug)]
pub struct AgentProfileRepo {
    pool: PgPool,
}

impl AgentProfileRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create a profile. [`StoreError::Conflict`] if `name` already exists in the
    /// workspace (the `(workspace_id, name)` unique key).
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        profile: &NewAgentProfile,
    ) -> Result<AgentProfile> {
        let row: AgentProfileRow = sqlx::query_as(&format!(
            "INSERT INTO agent_profiles
                 (id, workspace_id, name, model, system_prompt, tools, skills, subagents,
                  channels, grant_id, guard)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
             RETURNING {AGENT_PROFILE_COLS}"
        ))
        .bind(AgentProfileId::new().into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(&profile.name)
        .bind(&profile.model)
        .bind(&profile.system_prompt)
        .bind(Json(profile.tools.clone()))
        .bind(Json(profile.skills.clone()))
        .bind(Json(profile.subagents.clone()))
        .bind(Json(profile.channels.clone()))
        .bind(profile.grant_id.map(GrantId::into_uuid))
        .bind(profile.guard.as_ref().map(Json))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Idempotently upsert a profile by `(workspace_id, name)` — create it, or
    /// replace its definition (create-or-replace REST semantics).
    pub async fn upsert_by_name(
        &self,
        workspace_id: WorkspaceId,
        profile: &NewAgentProfile,
    ) -> Result<AgentProfile> {
        let row: AgentProfileRow = sqlx::query_as(&format!(
            "INSERT INTO agent_profiles
                 (id, workspace_id, name, model, system_prompt, tools, skills, subagents,
                  channels, grant_id, guard)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
             ON CONFLICT (workspace_id, name) DO UPDATE SET
                 model = EXCLUDED.model,
                 system_prompt = EXCLUDED.system_prompt,
                 tools = EXCLUDED.tools,
                 skills = EXCLUDED.skills,
                 subagents = EXCLUDED.subagents,
                 channels = EXCLUDED.channels,
                 grant_id = EXCLUDED.grant_id,
                 guard = EXCLUDED.guard,
                 updated_at = CURRENT_TIMESTAMP
             RETURNING {AGENT_PROFILE_COLS}"
        ))
        .bind(AgentProfileId::new().into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(&profile.name)
        .bind(&profile.model)
        .bind(&profile.system_prompt)
        .bind(Json(profile.tools.clone()))
        .bind(Json(profile.skills.clone()))
        .bind(Json(profile.subagents.clone()))
        .bind(Json(profile.channels.clone()))
        .bind(profile.grant_id.map(GrantId::into_uuid))
        .bind(profile.guard.as_ref().map(Json))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch a profile by id, workspace-scoped.
    pub async fn get(&self, workspace_id: WorkspaceId, id: AgentProfileId) -> Result<AgentProfile> {
        let row: AgentProfileRow = sqlx::query_as(&format!(
            "SELECT {AGENT_PROFILE_COLS} FROM agent_profiles WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch a profile by name (how the API + the `delegate` tool resolve it), or
    /// `None`.
    pub async fn get_by_name(
        &self,
        workspace_id: WorkspaceId,
        name: &str,
    ) -> Result<Option<AgentProfile>> {
        let row: Option<AgentProfileRow> = sqlx::query_as(&format!(
            "SELECT {AGENT_PROFILE_COLS} FROM agent_profiles WHERE workspace_id = $1 AND name = $2"
        ))
        .bind(workspace_id.into_uuid())
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.map(AgentProfile::from))
    }

    /// List a workspace's profiles, by name.
    pub async fn list_by_workspace(&self, workspace_id: WorkspaceId) -> Result<Vec<AgentProfile>> {
        let rows: Vec<AgentProfileRow> = sqlx::query_as(&format!(
            "SELECT {AGENT_PROFILE_COLS} FROM agent_profiles WHERE workspace_id = $1 \
             ORDER BY name ASC, id ASC"
        ))
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(AgentProfile::from).collect())
    }

    /// List the profiles **listening on** `channel` (its name is a member of the
    /// `channels` JSONB array), workspace-scoped — the channel→profile inbound
    /// routing lookup (SOUL §25). `@>` containment checks the array holds the name.
    pub async fn list_by_channel(
        &self,
        workspace_id: WorkspaceId,
        channel: &str,
    ) -> Result<Vec<AgentProfile>> {
        let rows: Vec<AgentProfileRow> = sqlx::query_as(&format!(
            "SELECT {AGENT_PROFILE_COLS} FROM agent_profiles \
             WHERE workspace_id = $1 AND channels @> $2 \
             ORDER BY name ASC, id ASC"
        ))
        .bind(workspace_id.into_uuid())
        .bind(Json(vec![channel.to_string()]))
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(AgentProfile::from).collect())
    }

    /// Delete a profile, workspace-scoped. [`StoreError::NotFound`] if absent.
    pub async fn delete(&self, workspace_id: WorkspaceId, id: AgentProfileId) -> Result<()> {
        let res = sqlx::query("DELETE FROM agent_profiles WHERE id = $1 AND workspace_id = $2")
            .bind(id.into_uuid())
            .bind(workspace_id.into_uuid())
            .execute(&self.pool)
            .await
            .map_err(map)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }
}

const MCP_SERVER_COLS: &str = "id, workspace_id, name, transport, command, args, env, url, auth, \
     enabled, tools, created_at, updated_at";

/// The fields to create or upsert an [`McpServerDef`] (SOUL §26). `args`/`env`/
/// `auth`/`tools` ride as JSONB; `auth` carries credentials verbatim, so a
/// `NewMcpServerDef` is sensitive — never log it.
#[derive(Clone, Debug)]
pub struct NewMcpServerDef {
    pub name: String,
    pub transport: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: std::collections::BTreeMap<String, String>,
    pub url: String,
    pub auth: catalerum_core::model::McpAuthSpec,
    pub enabled: bool,
    pub tools: Vec<String>,
}

/// CRUD for the `mcp_servers` table (SOUL §26). Every query is workspace-scoped
/// (§18). Mirrors [`AgentProfileRepo`].
#[derive(Clone, Debug)]
pub struct McpServerRepo {
    pool: PgPool,
}

impl McpServerRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create a server. [`StoreError::Conflict`] if `name` already exists in the
    /// workspace (the `(workspace_id, name)` unique key).
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        server: &NewMcpServerDef,
    ) -> Result<McpServerDef> {
        let row: McpServerRow = sqlx::query_as(&format!(
            "INSERT INTO mcp_servers
                 (id, workspace_id, name, transport, command, args, env, url, auth, enabled, tools)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
             RETURNING {MCP_SERVER_COLS}"
        ))
        .bind(McpServerId::new().into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(&server.name)
        .bind(&server.transport)
        .bind(&server.command)
        .bind(Json(server.args.clone()))
        .bind(Json(server.env.clone()))
        .bind(&server.url)
        .bind(Json(server.auth.clone()))
        .bind(server.enabled)
        .bind(Json(server.tools.clone()))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Idempotently upsert a server by `(workspace_id, name)` — create it, or
    /// replace its definition (the `edit_mcp_server` create-or-replace semantics).
    pub async fn upsert_by_name(
        &self,
        workspace_id: WorkspaceId,
        server: &NewMcpServerDef,
    ) -> Result<McpServerDef> {
        let row: McpServerRow = sqlx::query_as(&format!(
            "INSERT INTO mcp_servers
                 (id, workspace_id, name, transport, command, args, env, url, auth, enabled, tools)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
             ON CONFLICT (workspace_id, name) DO UPDATE SET
                 transport = EXCLUDED.transport,
                 command = EXCLUDED.command,
                 args = EXCLUDED.args,
                 env = EXCLUDED.env,
                 url = EXCLUDED.url,
                 auth = EXCLUDED.auth,
                 enabled = EXCLUDED.enabled,
                 tools = EXCLUDED.tools,
                 updated_at = CURRENT_TIMESTAMP
             RETURNING {MCP_SERVER_COLS}"
        ))
        .bind(McpServerId::new().into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(&server.name)
        .bind(&server.transport)
        .bind(&server.command)
        .bind(Json(server.args.clone()))
        .bind(Json(server.env.clone()))
        .bind(&server.url)
        .bind(Json(server.auth.clone()))
        .bind(server.enabled)
        .bind(Json(server.tools.clone()))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch a server by name, workspace-scoped (how the `*_mcp_server` tools
    /// resolve it), or `None`.
    pub async fn get_by_name(
        &self,
        workspace_id: WorkspaceId,
        name: &str,
    ) -> Result<Option<McpServerDef>> {
        let row: Option<McpServerRow> = sqlx::query_as(&format!(
            "SELECT {MCP_SERVER_COLS} FROM mcp_servers WHERE workspace_id = $1 AND name = $2"
        ))
        .bind(workspace_id.into_uuid())
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.map(McpServerDef::from))
    }

    /// List a workspace's servers, by name.
    pub async fn list_by_workspace(&self, workspace_id: WorkspaceId) -> Result<Vec<McpServerDef>> {
        let rows: Vec<McpServerRow> = sqlx::query_as(&format!(
            "SELECT {MCP_SERVER_COLS} FROM mcp_servers WHERE workspace_id = $1 \
             ORDER BY name ASC, id ASC"
        ))
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(McpServerDef::from).collect())
    }

    /// List every **enabled** server across all workspaces — the boot-time load
    /// that reconnects DB-defined servers (SOUL §26). Not workspace-scoped: it
    /// runs at startup before any request, to rebuild the live tool set.
    pub async fn list_enabled(&self) -> Result<Vec<McpServerDef>> {
        let rows: Vec<McpServerRow> = sqlx::query_as(&format!(
            "SELECT {MCP_SERVER_COLS} FROM mcp_servers WHERE enabled = TRUE \
             ORDER BY workspace_id ASC, name ASC, id ASC"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(McpServerDef::from).collect())
    }

    /// Delete a server by name, workspace-scoped. [`StoreError::NotFound`] if
    /// absent.
    pub async fn delete_by_name(&self, workspace_id: WorkspaceId, name: &str) -> Result<()> {
        let res = sqlx::query("DELETE FROM mcp_servers WHERE workspace_id = $1 AND name = $2")
            .bind(workspace_id.into_uuid())
            .bind(name)
            .execute(&self.pool)
            .await
            .map_err(map)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }
}

const TERMINAL_SESSION_COLS: &str = "id, workspace_id, backend, status, \
     host_dir, sync_prefix, pod_id, created_at, closed_at";

/// The fields to record a new [`TerminalSession`] (SOUL §20).
#[derive(Clone, Debug)]
pub struct NewTerminalSession {
    pub backend: ExecutorKind,
    pub host_dir: Option<String>,
    pub sync_prefix: Option<String>,
    /// The pod (process) that owns this session's node-local PTY (multi-pod HA,
    /// SOUL §16 M7). `None` only for a caller that predates pod ownership.
    pub pod_id: Option<String>,
}

/// CRUD for the `terminal_sessions` table (SOUL §20). Every query is
/// workspace-scoped (§18).
#[derive(Clone, Debug)]
pub struct TerminalSessionRepo {
    pool: PgPool,
}

impl TerminalSessionRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Record a new (active) session.
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        session: &NewTerminalSession,
    ) -> Result<TerminalSession> {
        let row: TerminalSessionRow = sqlx::query_as(&format!(
            "INSERT INTO terminal_sessions
                 (id, workspace_id, backend, host_dir, sync_prefix, pod_id)
             VALUES ($1, $2, $3, $4, $5, $6)
             RETURNING {TERMINAL_SESSION_COLS}"
        ))
        .bind(TerminalSessionId::new().into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(session.backend.as_token())
        .bind(&session.host_dir)
        .bind(&session.sync_prefix)
        .bind(&session.pod_id)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Fetch a session by id, workspace-scoped, or `None`.
    pub async fn get(
        &self,
        workspace_id: WorkspaceId,
        id: TerminalSessionId,
    ) -> Result<Option<TerminalSession>> {
        let row: Option<TerminalSessionRow> = sqlx::query_as(&format!(
            "SELECT {TERMINAL_SESSION_COLS} FROM terminal_sessions \
             WHERE workspace_id = $1 AND id = $2"
        ))
        .bind(workspace_id.into_uuid())
        .bind(id.into_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        row.map(TryInto::try_into).transpose()
    }

    /// Update a session's lifecycle status, stamping `closed_at` when it leaves
    /// the active state. Workspace-scoped; [`StoreError::NotFound`] if absent.
    pub async fn set_status(
        &self,
        workspace_id: WorkspaceId,
        id: TerminalSessionId,
        status: TerminalSessionStatus,
    ) -> Result<TerminalSession> {
        let closed = !matches!(status, TerminalSessionStatus::Active);
        let row: TerminalSessionRow = sqlx::query_as(&format!(
            "UPDATE terminal_sessions
                SET status = $3,
                    closed_at = CASE WHEN $4 THEN CURRENT_TIMESTAMP ELSE closed_at END
              WHERE workspace_id = $1 AND id = $2
             RETURNING {TERMINAL_SESSION_COLS}"
        ))
        .bind(workspace_id.into_uuid())
        .bind(id.into_uuid())
        .bind(terminal_session_status_to_text(status))
        .bind(closed)
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?
        .ok_or(StoreError::NotFound)?;
        row.try_into()
    }

    /// Record the object-storage key prefix a session was last persisted under.
    pub async fn set_sync_prefix(
        &self,
        workspace_id: WorkspaceId,
        id: TerminalSessionId,
        prefix: &str,
    ) -> Result<()> {
        let res = sqlx::query(
            "UPDATE terminal_sessions SET sync_prefix = $3 WHERE workspace_id = $1 AND id = $2",
        )
        .bind(workspace_id.into_uuid())
        .bind(id.into_uuid())
        .bind(prefix)
        .execute(&self.pool)
        .await
        .map_err(map)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// List a workspace's sessions, newest first.
    pub async fn list_by_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<TerminalSession>> {
        let rows: Vec<TerminalSessionRow> = sqlx::query_as(&format!(
            "SELECT {TERMINAL_SESSION_COLS} FROM terminal_sessions WHERE workspace_id = $1 \
             ORDER BY created_at DESC, id ASC"
        ))
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(TryInto::try_into).collect()
    }

    /// List a workspace's **active** sessions, newest first.
    pub async fn list_active(&self, workspace_id: WorkspaceId) -> Result<Vec<TerminalSession>> {
        let rows: Vec<TerminalSessionRow> = sqlx::query_as(&format!(
            "SELECT {TERMINAL_SESSION_COLS} FROM terminal_sessions \
             WHERE workspace_id = $1 AND status = 'active' \
             ORDER BY created_at DESC, id ASC"
        ))
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(TryInto::try_into).collect()
    }

    /// Mark **every** still-`active` session as closed, stamping `closed_at`.
    /// A boot/maintenance operation and the one query here that is deliberately
    /// **not** workspace-scoped (§18): the interactive PTY / container / Pod
    /// handles all live in the `TerminalManager`'s process memory, so a process
    /// restart orphans every active row — none can survive. Returns how many
    /// rows were reconciled. Idempotent (a second call closes nothing).
    pub async fn close_all_active(&self) -> Result<u64> {
        let res = sqlx::query(
            "UPDATE terminal_sessions \
                SET status = 'closed', closed_at = CURRENT_TIMESTAMP \
              WHERE status = 'active'",
        )
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(res.rows_affected())
    }

    /// Boot reconcile **scoped to one owning pod** (multi-pod HA, SOUL §16 M7).
    /// Closes every still-`active` session that either belongs to `pod_id` (this
    /// restarting process — its in-memory PTYs died with the prior run) or has a
    /// NULL `pod_id` (a pre-upgrade orphan whose owner is unknowable; whichever
    /// pod boots first reclaims it, harmlessly). A peer pod's rows are left
    /// untouched so a rolling restart never marks another pod's live sessions
    /// closed. Returns how many rows were reconciled; idempotent.
    pub async fn close_all_active_for_pod(&self, pod_id: &str) -> Result<u64> {
        let res = sqlx::query(
            "UPDATE terminal_sessions \
                SET status = 'closed', closed_at = CURRENT_TIMESTAMP \
              WHERE status = 'active' AND (pod_id = $1 OR pod_id IS NULL)",
        )
        .bind(pod_id)
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(res.rows_affected())
    }

    /// Stale-pod reclaim (pod-heartbeat follow-up, SOUL §20/§16 M7): close every
    /// still-`active` session whose owning pod is **provably dead** — it has a
    /// [`pod_heartbeats`](PodHeartbeatRepo) row that has gone stale (its
    /// `last_seen` is older than `grace`). Rows only: the dead pod's PTYs/containers
    /// died with it, so this just clears the phantom `active` row a permanently-dead
    /// pod leaves behind (the common case under a Deployment, where a replaced pod
    /// never returns under its old HOSTNAME).
    ///
    /// # The never-heartbeated safety rule
    /// This matches only pods that HAVE a stale heartbeat row (`pod_id IN (SELECT …
    /// WHERE last_seen < cutoff)`), **never** a `pod_id` with no heartbeat row at
    /// all. Two cases motivate this:
    /// * A **freshly-booted** pod writes its first heartbeat before it can create
    ///   any session (main.rs stamps it before boot reconcile / before serving), so
    ///   its rows never look stale — and even in the boot window before that write,
    ///   the absent-row rule protects them.
    /// * A pod running **pre-heartbeat code** during a rolling upgrade never writes
    ///   a heartbeat; its still-live rows must not be swept. Leaving never-heartbeated
    ///   pods to the legacy NULL / per-pod boot-reconcile path
    ///   ([`close_all_active_for_pod`](Self::close_all_active_for_pod)) is the
    ///   conservative choice.
    ///
    /// `grace` must be generously larger than the heartbeat interval (a
    /// paused-but-alive pod that resumes within `grace` keeps its rows). Runs on
    /// every pod but is naturally **idempotent**: it only matches `status =
    /// 'active'` rows, so a second run (or a concurrent peer's run) closes nothing —
    /// no bus lock needed. Returns how many rows were reclaimed.
    pub async fn reclaim_stale_for_dead_pods(&self, grace: Duration) -> Result<u64> {
        let cutoff = cutoff_before(grace);
        #[cfg(not(feature = "sqlite"))]
        let heartbeat_expired = "last_seen < $1";
        #[cfg(feature = "sqlite")]
        let heartbeat_expired = "julianday(last_seen) < julianday($1)";
        let res = sqlx::query(&format!(
            "UPDATE terminal_sessions \
                SET status = 'closed', closed_at = CURRENT_TIMESTAMP \
              WHERE status = 'active' \
                AND pod_id IS NOT NULL \
                AND pod_id IN ( \
                    SELECT pod_id FROM pod_heartbeats \
                     WHERE {heartbeat_expired} \
                )"
        ))
        .bind(cutoff)
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(res.rows_affected())
    }

    /// Delete a session by id, workspace-scoped. [`StoreError::NotFound`] if absent.
    pub async fn delete(&self, workspace_id: WorkspaceId, id: TerminalSessionId) -> Result<()> {
        let res = sqlx::query("DELETE FROM terminal_sessions WHERE workspace_id = $1 AND id = $2")
            .bind(workspace_id.into_uuid())
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

const WORKSPACE_SANDBOX_COLS: &str = "workspace_id, backend, image, status, container_ref, \
     volume_ref, pod_id, last_activity, created_at, updated_at";

/// The fields to upsert a [`WorkspaceSandboxRecord`] (SOUL §20).
#[derive(Clone, Debug)]
pub struct NewWorkspaceSandbox {
    pub backend: ExecutorKind,
    pub image: String,
    pub status: SandboxState,
    pub container_ref: Option<String>,
    pub volume_ref: Option<String>,
    /// The pod (process) provisioning this sandbox's node-local container/Pod
    /// (multi-pod HA, SOUL §16 M7). `None` only for a caller that predates it.
    pub pod_id: Option<String>,
}

/// CRUD for the `workspace_sandboxes` table (SOUL §20). Exactly one row per
/// workspace (`workspace_id` is the primary key); every query is
/// workspace-scoped (§18).
#[derive(Clone, Debug)]
pub struct WorkspaceSandboxRepo {
    pool: PgPool,
}

impl WorkspaceSandboxRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Idempotently upsert a workspace's sandbox record by `workspace_id`,
    /// refreshing `last_activity` and `updated_at`.
    pub async fn upsert(
        &self,
        workspace_id: WorkspaceId,
        sandbox: &NewWorkspaceSandbox,
    ) -> Result<WorkspaceSandboxRecord> {
        let row: WorkspaceSandboxRow = sqlx::query_as(&format!(
            "INSERT INTO workspace_sandboxes
                 (workspace_id, backend, image, status, container_ref, volume_ref, pod_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (workspace_id) DO UPDATE SET
                 backend = EXCLUDED.backend,
                 image = EXCLUDED.image,
                 status = EXCLUDED.status,
                 container_ref = EXCLUDED.container_ref,
                 volume_ref = EXCLUDED.volume_ref,
                 pod_id = EXCLUDED.pod_id,
                 last_activity = CURRENT_TIMESTAMP,
                 updated_at = CURRENT_TIMESTAMP
             RETURNING {WORKSPACE_SANDBOX_COLS}"
        ))
        .bind(workspace_id.into_uuid())
        .bind(sandbox.backend.as_token())
        .bind(&sandbox.image)
        .bind(sandbox_state_to_text(sandbox.status))
        .bind(&sandbox.container_ref)
        .bind(&sandbox.volume_ref)
        .bind(&sandbox.pod_id)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Fetch a workspace's sandbox record, or `None`.
    pub async fn get(&self, workspace_id: WorkspaceId) -> Result<Option<WorkspaceSandboxRecord>> {
        let row: Option<WorkspaceSandboxRow> = sqlx::query_as(&format!(
            "SELECT {WORKSPACE_SANDBOX_COLS} FROM workspace_sandboxes WHERE workspace_id = $1"
        ))
        .bind(workspace_id.into_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        row.map(TryInto::try_into).transpose()
    }

    /// Update a workspace sandbox's lifecycle status. [`StoreError::NotFound`] if
    /// no row exists.
    pub async fn set_status(
        &self,
        workspace_id: WorkspaceId,
        status: SandboxState,
    ) -> Result<WorkspaceSandboxRecord> {
        let row: WorkspaceSandboxRow = sqlx::query_as(&format!(
            "UPDATE workspace_sandboxes SET status = $2, updated_at = CURRENT_TIMESTAMP \
              WHERE workspace_id = $1 \
             RETURNING {WORKSPACE_SANDBOX_COLS}"
        ))
        .bind(workspace_id.into_uuid())
        .bind(sandbox_state_to_text(status))
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?
        .ok_or(StoreError::NotFound)?;
        row.try_into()
    }

    /// Bump a workspace sandbox's `last_activity` to now (drives idle reaping).
    /// [`StoreError::NotFound`] if no row exists.
    pub async fn touch_activity(&self, workspace_id: WorkspaceId) -> Result<()> {
        let res = sqlx::query(
            "UPDATE workspace_sandboxes SET last_activity = CURRENT_TIMESTAMP, updated_at = CURRENT_TIMESTAMP \
              WHERE workspace_id = $1",
        )
        .bind(workspace_id.into_uuid())
        .execute(&self.pool)
        .await
        .map_err(map)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// List every sandbox record (all workspaces). For observability + the boot
    /// reconcile; not workspace-scoped by design.
    pub async fn list_all(&self) -> Result<Vec<WorkspaceSandboxRecord>> {
        let rows: Vec<WorkspaceSandboxRow> = sqlx::query_as(&format!(
            "SELECT {WORKSPACE_SANDBOX_COLS} FROM workspace_sandboxes ORDER BY created_at ASC"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(TryInto::try_into).collect()
    }

    /// Delete a workspace's sandbox record. [`StoreError::NotFound`] if absent.
    pub async fn delete(&self, workspace_id: WorkspaceId) -> Result<()> {
        let res = sqlx::query("DELETE FROM workspace_sandboxes WHERE workspace_id = $1")
            .bind(workspace_id.into_uuid())
            .execute(&self.pool)
            .await
            .map_err(map)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// Mark **every** non-stopped sandbox row `stopped` — a boot reconcile (the
    /// live container/Pod handles live in the API process memory, so a restart
    /// orphans them; on podman the deterministic name lets `ensure` re-adopt).
    /// Returns how many rows were reconciled. Deliberately not workspace-scoped.
    pub async fn mark_all_stopped(&self) -> Result<u64> {
        let res = sqlx::query(
            "UPDATE workspace_sandboxes SET status = 'stopped', updated_at = CURRENT_TIMESTAMP \
              WHERE status <> 'stopped'",
        )
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(res.rows_affected())
    }

    /// Boot reconcile **scoped to one owning pod** (multi-pod HA, SOUL §16 M7).
    /// Marks `stopped` every non-stopped sandbox row that either belongs to
    /// `pod_id` (this restarting process — its container/Pod handle died with the
    /// prior run; podman re-adopts by name on the next `ensure`) or has a NULL
    /// `pod_id` (a pre-upgrade orphan). A peer pod's rows are left untouched, so a
    /// rolling restart never stomps another pod's live sandbox. Returns how many
    /// rows were reconciled; idempotent.
    pub async fn mark_all_stopped_for_pod(&self, pod_id: &str) -> Result<u64> {
        let res = sqlx::query(
            "UPDATE workspace_sandboxes SET status = 'stopped', updated_at = CURRENT_TIMESTAMP \
              WHERE status <> 'stopped' AND (pod_id = $1 OR pod_id IS NULL)",
        )
        .bind(pod_id)
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(res.rows_affected())
    }

    /// Stale-pod reclaim (pod-heartbeat follow-up, SOUL §20/§16 M7): mark `stopped`
    /// every non-stopped sandbox row whose owning pod is **provably dead** — it has
    /// a [`pod_heartbeats`](PodHeartbeatRepo) row that has gone stale (older than
    /// `grace`). The sandbox row is the api-layer bookkeeping; the dead pod's
    /// container/Pod handle died with it (on podman a deterministic name lets a
    /// future `ensure` re-adopt). Uses the same **never-heartbeated safety rule** as
    /// [`TerminalSessionRepo::reclaim_stale_for_dead_pods`] (see its docs): a
    /// `pod_id` with no heartbeat row is never swept. Naturally idempotent (only
    /// `status <> 'stopped'` rows match), so it needs no bus lock and is safe to run
    /// on every pod. Returns how many rows were reclaimed.
    pub async fn reclaim_stale_for_dead_pods(&self, grace: Duration) -> Result<u64> {
        let cutoff = cutoff_before(grace);
        #[cfg(not(feature = "sqlite"))]
        let heartbeat_expired = "last_seen < $1";
        #[cfg(feature = "sqlite")]
        let heartbeat_expired = "julianday(last_seen) < julianday($1)";
        let res = sqlx::query(&format!(
            "UPDATE workspace_sandboxes SET status = 'stopped', updated_at = CURRENT_TIMESTAMP \
              WHERE status <> 'stopped' \
                AND pod_id IS NOT NULL \
                AND pod_id IN ( \
                    SELECT pod_id FROM pod_heartbeats \
                     WHERE {heartbeat_expired} \
                )"
        ))
        .bind(cutoff)
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(res.rows_affected())
    }
}

/// CRUD for the `pod_heartbeats` table (pod-heartbeat follow-up, SOUL §20/§16 M7):
/// a per-process liveness signal that lets the stale-pod reclaim sweeps
/// ([`TerminalSessionRepo::reclaim_stale_for_dead_pods`] /
/// [`WorkspaceSandboxRepo::reclaim_stale_for_dead_pods`]) tell a permanently-dead
/// pod apart from a paused-but-alive one. Deliberately not workspace-scoped — a pod
/// spans every workspace it hosts sessions for.
#[derive(Clone, Debug)]
pub struct PodHeartbeatRepo {
    pool: PgPool,
}

impl PodHeartbeatRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Upsert this process's heartbeat, stamping `last_seen = CURRENT_TIMESTAMP`. Called on a
    /// short interval (~30 s) by every pod. Cheap (a single-row upsert on a tiny
    /// table); the caller logs-and-continues on error rather than crashing.
    pub async fn heartbeat(&self, pod_id: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO pod_heartbeats (pod_id, last_seen) VALUES ($1, CURRENT_TIMESTAMP) \
             ON CONFLICT (pod_id) DO UPDATE SET last_seen = CURRENT_TIMESTAMP",
        )
        .bind(pod_id)
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(())
    }

    /// Fetch a pod's last-seen timestamp, or `None` if it has never heartbeated.
    /// For observability + tests.
    pub async fn last_seen(&self, pod_id: &str) -> Result<Option<DateTime<Utc>>> {
        let row: Option<(DateTime<Utc>,)> =
            sqlx::query_as("SELECT last_seen FROM pod_heartbeats WHERE pod_id = $1")
                .bind(pod_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(map)?;
        Ok(row.map(|(ts,)| ts))
    }

    /// Delete heartbeat rows not seen within `horizon`, so the table can't grow
    /// unbounded as pods come and go (e.g. a 7-day horizon). By the time a dead
    /// pod's row ages past the horizon its owned session/sandbox rows are long
    /// reclaimed, so pruning it is purely housekeeping. Returns rows deleted.
    pub async fn prune(&self, horizon: Duration) -> Result<u64> {
        let cutoff = cutoff_before(horizon);
        #[cfg(not(feature = "sqlite"))]
        let expired = "last_seen < $1";
        #[cfg(feature = "sqlite")]
        let expired = "julianday(last_seen) < julianday($1)";
        let res = sqlx::query(&format!("DELETE FROM pod_heartbeats WHERE {expired}"))
            .bind(cutoff)
            .execute(&self.pool)
            .await
            .map_err(map)?;
        Ok(res.rows_affected())
    }
}

const AUTOMATION_COLS: &str = "id, workspace_id, name, enabled, triggers, condition, actions, \
     spec, grant_id, created_at, updated_at";

/// The fields to create or upsert an [`Automation`] (SOUL §11). The JSON `triggers`,
/// `condition`, `actions`, and `spec` are stored verbatim as JSONB — the
/// `catalerum-automation` engine owns their typed meaning.
#[derive(Clone, Debug)]
pub struct NewAutomation {
    /// Unique (per workspace) automation name.
    pub name: String,
    /// Whether the automation is active. Disabled automations persist but do not fire.
    pub enabled: bool,
    /// Trigger specs (CalendarEvent / Schedule / Webhook / …).
    pub triggers: Vec<serde_json::Value>,
    /// Optional predicate over store/graph/vectors; `None` fires unconditionally.
    pub condition: Option<serde_json::Value>,
    /// Ordered typed action specs (also the LLM's tools, §11).
    pub actions: Vec<serde_json::Value>,
    /// The full original authoring spec.
    pub spec: Option<serde_json::Value>,
    /// The §19 grant the automation runs under.
    pub grant_id: Option<GrantId>,
}

/// CRUD for the `automations` table (SOUL §11). Automations are named,
/// workspace-scoped definitions; the engine (`catalerum-automation`) reads them
/// to register triggers and dispatch runs.
#[derive(Clone, Debug)]
pub struct AutomationRepo {
    pool: PgPool,
}

impl AutomationRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create an automation. [`StoreError::Conflict`] if `name` already exists in
    /// the workspace (the `(workspace_id, name)` unique key).
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        automation: &NewAutomation,
    ) -> Result<Automation> {
        let row: AutomationRow = sqlx::query_as(&format!(
            "INSERT INTO automations
                 (id, workspace_id, name, enabled, triggers, condition, actions, spec, grant_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             RETURNING {AUTOMATION_COLS}"
        ))
        .bind(AutomationId::new().into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(&automation.name)
        .bind(automation.enabled)
        .bind(Json(automation.triggers.clone()))
        .bind(automation.condition.as_ref().map(Json))
        .bind(Json(automation.actions.clone()))
        .bind(automation.spec.as_ref().map(Json))
        .bind(automation.grant_id.map(GrantId::into_uuid))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Idempotently upsert an automation by `(workspace_id, name)` — create it, or
    /// replace its definition (keeping the stable id, §11).
    pub async fn upsert_by_name(
        &self,
        workspace_id: WorkspaceId,
        automation: &NewAutomation,
    ) -> Result<Automation> {
        let row: AutomationRow = sqlx::query_as(&format!(
            "INSERT INTO automations
                 (id, workspace_id, name, enabled, triggers, condition, actions, spec, grant_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT (workspace_id, name) DO UPDATE SET
                 enabled = EXCLUDED.enabled,
                 triggers = EXCLUDED.triggers,
                 condition = EXCLUDED.condition,
                 actions = EXCLUDED.actions,
                 spec = EXCLUDED.spec,
                 grant_id = EXCLUDED.grant_id,
                 updated_at = CURRENT_TIMESTAMP
             RETURNING {AUTOMATION_COLS}"
        ))
        .bind(AutomationId::new().into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(&automation.name)
        .bind(automation.enabled)
        .bind(Json(automation.triggers.clone()))
        .bind(automation.condition.as_ref().map(Json))
        .bind(Json(automation.actions.clone()))
        .bind(automation.spec.as_ref().map(Json))
        .bind(automation.grant_id.map(GrantId::into_uuid))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch an automation by id, workspace-scoped.
    pub async fn get(&self, workspace_id: WorkspaceId, id: AutomationId) -> Result<Automation> {
        let row: AutomationRow = sqlx::query_as(&format!(
            "SELECT {AUTOMATION_COLS} FROM automations WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch an automation by name, workspace-scoped, or `None`.
    pub async fn get_by_name(
        &self,
        workspace_id: WorkspaceId,
        name: &str,
    ) -> Result<Option<Automation>> {
        let row: Option<AutomationRow> = sqlx::query_as(&format!(
            "SELECT {AUTOMATION_COLS} FROM automations WHERE workspace_id = $1 AND name = $2"
        ))
        .bind(workspace_id.into_uuid())
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.map(Automation::from))
    }

    /// List a workspace's automations, by name.
    pub async fn list_by_workspace(&self, workspace_id: WorkspaceId) -> Result<Vec<Automation>> {
        let rows: Vec<AutomationRow> = sqlx::query_as(&format!(
            "SELECT {AUTOMATION_COLS} FROM automations WHERE workspace_id = $1 ORDER BY name ASC, id ASC"
        ))
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Automation::from).collect())
    }

    /// Enable or disable an automation, workspace-scoped. Returns the updated row;
    /// [`StoreError::NotFound`] if absent. The cheap toggle the engine and UI use
    /// to pause/resume without rewriting the definition.
    pub async fn set_enabled(
        &self,
        workspace_id: WorkspaceId,
        id: AutomationId,
        enabled: bool,
    ) -> Result<Automation> {
        let row: AutomationRow = sqlx::query_as(&format!(
            "UPDATE automations SET enabled = $3, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2
             RETURNING {AUTOMATION_COLS}"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(enabled)
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?
        .ok_or(StoreError::NotFound)?;
        Ok(row.into())
    }

    /// Delete an automation, workspace-scoped. [`StoreError::NotFound`] if absent.
    pub async fn delete(&self, workspace_id: WorkspaceId, id: AutomationId) -> Result<()> {
        let res = sqlx::query("DELETE FROM automations WHERE id = $1 AND workspace_id = $2")
            .bind(id.into_uuid())
            .bind(workspace_id.into_uuid())
            .execute(&self.pool)
            .await
            .map_err(map)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }
}

const RUN_COLS: &str =
    "id, workspace_id, automation_id, status, grant_id, trigger, error, started_at, finished_at";
const STEP_COLS: &str =
    "id, run_id, workspace_id, ordinal, action, status, output, error, started_at, finished_at";

/// Durable run/step state for automations (SOUL §11). The engine opens a run when
/// a matched trigger fires ([`start_run`](AutomationRunRepo::start_run)), records
/// each action as a step, and finalizes both — the audit trail that survives
/// crashes and drives reconciliation. Every method is workspace-scoped (§18), and
/// run/step inserts are guarded so a run can only be opened for an automation in
/// the caller's workspace, and a step only appended to a run in it.
#[derive(Clone, Debug)]
pub struct AutomationRunRepo {
    pool: PgPool,
}

impl AutomationRunRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Open a `Running` run for `automation_id` (SOUL §11). [`StoreError::NotFound`]
    /// if the automation does not exist in `workspace_id` (the tenancy guard).
    ///
    /// `grant_id` is the §19 grant the run executes under, snapshotted onto the run
    /// for audit (which authority was in force for this execution); `None` for a run
    /// under default base authority. It is recorded as a plain value (no FK) so the
    /// audit fact survives the grant's later deletion.
    pub async fn start_run(
        &self,
        workspace_id: WorkspaceId,
        automation_id: AutomationId,
        grant_id: Option<GrantId>,
        trigger: Option<serde_json::Value>,
        job_id: Option<Uuid>,
    ) -> Result<AutomationRun> {
        let row: Option<AutomationRunRow> = sqlx::query_as(&format!(
            "INSERT INTO automation_runs \
                 (id, workspace_id, automation_id, status, grant_id, trigger, job_id)
             SELECT $1, $2, $3, $4, $5, $6, $7
             WHERE EXISTS (SELECT 1 FROM automations WHERE id = $3 AND workspace_id = $2)
             RETURNING {RUN_COLS}"
        ))
        .bind(AutomationRunId::new().into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(automation_id.into_uuid())
        .bind(run_status_to_text(RunStatus::Running))
        .bind(grant_id.map(GrantId::into_uuid))
        .bind(trigger.as_ref().map(Json))
        .bind(job_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        row.ok_or(StoreError::NotFound)?.try_into()
    }

    /// The active (still-`running`) run spawned by `job_id`, if any (SOUL §5/§11).
    /// How a re-driven `run_automation` job finds the run it must **resume** rather
    /// than re-create after a crash. Newest first; `None` if the job has no live run
    /// (never started, or already finalized).
    pub async fn find_active_run_by_job(
        &self,
        workspace_id: WorkspaceId,
        job_id: Uuid,
    ) -> Result<Option<AutomationRunId>> {
        let row: Option<(Uuid,)> = sqlx::query_as(
            "SELECT id FROM automation_runs
             WHERE workspace_id = $1 AND job_id = $2 AND status = $3
             ORDER BY started_at DESC LIMIT 1",
        )
        .bind(workspace_id.into_uuid())
        .bind(job_id)
        .bind(run_status_to_text(RunStatus::Running))
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.map(|(id,)| AutomationRunId::from_uuid(id)))
    }

    /// Finalize a run with a terminal `status` (and optional failure `error`),
    /// stamping `finished_at`. [`StoreError::NotFound`] if absent.
    pub async fn finish_run(
        &self,
        workspace_id: WorkspaceId,
        run_id: AutomationRunId,
        status: RunStatus,
        error: Option<&str>,
    ) -> Result<AutomationRun> {
        let row: Option<AutomationRunRow> = sqlx::query_as(&format!(
            "UPDATE automation_runs SET status = $3, error = $4, finished_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2
             RETURNING {RUN_COLS}"
        ))
        .bind(run_id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(run_status_to_text(status))
        .bind(error)
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        row.ok_or(StoreError::NotFound)?.try_into()
    }

    /// Fetch a run by id, workspace-scoped.
    pub async fn get_run(
        &self,
        workspace_id: WorkspaceId,
        run_id: AutomationRunId,
    ) -> Result<AutomationRun> {
        let row: AutomationRunRow = sqlx::query_as(&format!(
            "SELECT {RUN_COLS} FROM automation_runs WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(run_id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// List an automation's runs, most recent first, capped at `limit`.
    pub async fn list_runs(
        &self,
        workspace_id: WorkspaceId,
        automation_id: AutomationId,
        limit: i64,
    ) -> Result<Vec<AutomationRun>> {
        let rows: Vec<AutomationRunRow> = sqlx::query_as(&format!(
            "SELECT {RUN_COLS} FROM automation_runs
             WHERE workspace_id = $1 AND automation_id = $2
             ORDER BY started_at DESC, id DESC LIMIT $3"
        ))
        .bind(workspace_id.into_uuid())
        .bind(automation_id.into_uuid())
        .bind(limit.max(0))
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(AutomationRun::try_from).collect()
    }

    /// Append a `Running` step to a run (SOUL §11). [`StoreError::NotFound`] if the
    /// run does not exist in `workspace_id`; [`StoreError::Conflict`] if `ordinal`
    /// is already used in the run (the `(run_id, ordinal)` unique key).
    pub async fn add_step(
        &self,
        workspace_id: WorkspaceId,
        run_id: AutomationRunId,
        ordinal: i32,
        action: serde_json::Value,
    ) -> Result<AutomationStep> {
        let row: Option<AutomationStepRow> = sqlx::query_as(&format!(
            "INSERT INTO automation_steps (id, run_id, workspace_id, ordinal, action, status)
             SELECT $1, $2, $3, $4, $5, $6
             WHERE EXISTS (SELECT 1 FROM automation_runs WHERE id = $2 AND workspace_id = $3)
             RETURNING {STEP_COLS}"
        ))
        .bind(AutomationStepId::new().into_uuid())
        .bind(run_id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(ordinal)
        .bind(Json(action))
        .bind(step_status_to_text(StepStatus::Running))
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        row.ok_or(StoreError::NotFound)?.try_into()
    }

    /// Finalize a step with a terminal `status`, optional `output`/`error`,
    /// stamping `finished_at`. [`StoreError::NotFound`] if absent.
    pub async fn finish_step(
        &self,
        workspace_id: WorkspaceId,
        step_id: AutomationStepId,
        status: StepStatus,
        output: Option<serde_json::Value>,
        error: Option<&str>,
    ) -> Result<AutomationStep> {
        let row: Option<AutomationStepRow> = sqlx::query_as(&format!(
            "UPDATE automation_steps SET status = $3, output = $4, error = $5, finished_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2
             RETURNING {STEP_COLS}"
        ))
        .bind(step_id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(step_status_to_text(status))
        .bind(output.as_ref().map(Json))
        .bind(error)
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        row.ok_or(StoreError::NotFound)?.try_into()
    }

    /// List a run's steps in execution order, workspace-scoped.
    pub async fn list_steps(
        &self,
        workspace_id: WorkspaceId,
        run_id: AutomationRunId,
    ) -> Result<Vec<AutomationStep>> {
        let rows: Vec<AutomationStepRow> = sqlx::query_as(&format!(
            "SELECT {STEP_COLS} FROM automation_steps
             WHERE workspace_id = $1 AND run_id = $2
             ORDER BY ordinal ASC, id ASC"
        ))
        .bind(workspace_id.into_uuid())
        .bind(run_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(AutomationStep::try_from).collect()
    }
}

// ===========================================================================
// External-DB manual migrations (SOUL §11) — ordered, hand-written SQL migrations
// authored for a specific external Postgres connection, plus the applied ledger
// (tracked in catalerum's own DB, keyed by connection).
// ===========================================================================

/// A registered manual migration for an external connection, with whether it has
/// been applied (from the ledger). Returned by [`ExternalDbMigrationRepo::list`].
#[derive(Clone, Debug, serde::Serialize)]
pub struct ExternalDbMigration {
    pub version: i64,
    pub name: String,
    pub up_sql: String,
    pub checksum: String,
    pub applied: bool,
}

#[derive(sqlx::FromRow)]
struct ExternalDbMigrationRow {
    version: i64,
    name: String,
    up_sql: String,
    checksum: String,
    applied: bool,
}

/// CRUD over the external-DB migration scripts + applied ledger (SOUL §11).
#[derive(Clone, Debug)]
pub struct ExternalDbMigrationRepo {
    pool: PgPool,
}

impl ExternalDbMigrationRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Register a manual migration script for `connection_id`. `version` is unique
    /// per connection ([`StoreError::Conflict`] on a duplicate).
    pub async fn add_script(
        &self,
        workspace_id: WorkspaceId,
        connection_id: ConnectionId,
        version: i64,
        name: &str,
        up_sql: &str,
        checksum: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO external_db_migration_scripts
                 (id, workspace_id, connection_id, version, name, up_sql, checksum)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(Uuid::new_v4())
        .bind(workspace_id.into_uuid())
        .bind(connection_id.into_uuid())
        .bind(version)
        .bind(name)
        .bind(up_sql)
        .bind(checksum)
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(())
    }

    /// List a connection's migration scripts in ascending version order, each with
    /// an `applied` flag from the ledger.
    pub async fn list(
        &self,
        workspace_id: WorkspaceId,
        connection_id: ConnectionId,
    ) -> Result<Vec<ExternalDbMigration>> {
        let rows: Vec<ExternalDbMigrationRow> = sqlx::query_as(
            "SELECT s.version, s.name, s.up_sql, s.checksum,
                    (m.version IS NOT NULL) AS applied
             FROM external_db_migration_scripts s
             LEFT JOIN external_db_migrations m
                 ON m.connection_id = s.connection_id AND m.version = s.version
             WHERE s.workspace_id = $1 AND s.connection_id = $2
             ORDER BY s.version ASC",
        )
        .bind(workspace_id.into_uuid())
        .bind(connection_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows
            .into_iter()
            .map(|r| ExternalDbMigration {
                version: r.version,
                name: r.name,
                up_sql: r.up_sql,
                checksum: r.checksum,
                applied: r.applied,
            })
            .collect())
    }

    /// Record a migration as applied in the ledger (idempotent via the
    /// `(connection_id, version)` unique key — a re-apply is a [`StoreError::Conflict`]).
    pub async fn record_applied(
        &self,
        workspace_id: WorkspaceId,
        connection_id: ConnectionId,
        version: i64,
        name: &str,
        checksum: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO external_db_migrations
                 (id, workspace_id, connection_id, version, name, checksum)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(Uuid::new_v4())
        .bind(workspace_id.into_uuid())
        .bind(connection_id.into_uuid())
        .bind(version)
        .bind(name)
        .bind(checksum)
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(())
    }
}

// ===========================================================================
// MCP endpoints (Boa-scripted scoped MCP endpoints, SOUL §26)
// ===========================================================================

/// The stored columns of an `mcp_endpoints` row, in one place so every query
/// projects them identically into a [`McpEndpointRow`].
const MCP_ENDPOINT_COLS: &str = "id, workspace_id, name, description, script, bucket_name, \
     key_prefix, grant_id, enabled, author_kind, author_id, created_at, updated_at";

/// The editable fields of an MCP endpoint — shared by `create` and `update`.
#[derive(Clone, Debug)]
pub struct McpEndpointInput {
    pub name: String,
    pub description: String,
    pub script: String,
    pub bucket_name: Option<String>,
    pub key_prefix: Option<String>,
    pub grant_id: Option<GrantId>,
    pub enabled: bool,
}

/// CRUD for the `mcp_endpoints` table — user-authored, Boa-scripted MCP endpoints
/// (SOUL §26). Every query is workspace-filtered (SOUL §18); `(workspace_id,
/// name)` is UNIQUE, so endpoints are addressed by name in the URL.
#[derive(Clone, Debug)]
pub struct McpEndpointRepo {
    pool: PgPool,
}

impl McpEndpointRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create an endpoint authored by `author`. A duplicate `(workspace, name)` is
    /// a [`StoreError::Conflict`].
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        author: Author,
        input: &McpEndpointInput,
    ) -> Result<McpEndpoint> {
        let (author_kind, author_id) = author_to_parts(author);
        let row: McpEndpointRow = sqlx::query_as(&format!(
            "INSERT INTO mcp_endpoints
                 (id, workspace_id, name, description, script, bucket_name, key_prefix,
                  grant_id, enabled, author_kind, author_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
             RETURNING {MCP_ENDPOINT_COLS}"
        ))
        .bind(McpEndpointId::new().into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(&input.name)
        .bind(&input.description)
        .bind(&input.script)
        .bind(input.bucket_name.as_deref())
        .bind(input.key_prefix.as_deref())
        .bind(input.grant_id.map(GrantId::into_uuid))
        .bind(input.enabled)
        .bind(author_kind)
        .bind(author_id)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Fetch an endpoint by id, scoped to its workspace.
    pub async fn get(&self, workspace_id: WorkspaceId, id: McpEndpointId) -> Result<McpEndpoint> {
        let row: McpEndpointRow = sqlx::query_as(&format!(
            "SELECT {MCP_ENDPOINT_COLS} FROM mcp_endpoints
             WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Fetch an endpoint by its (workspace-unique) name — the serve-time lookup.
    pub async fn get_by_name(&self, workspace_id: WorkspaceId, name: &str) -> Result<McpEndpoint> {
        let row: McpEndpointRow = sqlx::query_as(&format!(
            "SELECT {MCP_ENDPOINT_COLS} FROM mcp_endpoints
             WHERE workspace_id = $1 AND name = $2"
        ))
        .bind(workspace_id.into_uuid())
        .bind(name)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// List a workspace's endpoints, most-recently-edited first.
    pub async fn list_by_workspace(&self, workspace_id: WorkspaceId) -> Result<Vec<McpEndpoint>> {
        let rows: Vec<McpEndpointRow> = sqlx::query_as(&format!(
            "SELECT {MCP_ENDPOINT_COLS} FROM mcp_endpoints
             WHERE workspace_id = $1
             ORDER BY updated_at DESC, id ASC"
        ))
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(McpEndpoint::try_from).collect()
    }

    /// Update an endpoint's editable fields (author is immutable), bumping
    /// `updated_at`. A rename that collides is a [`StoreError::Conflict`].
    pub async fn update(
        &self,
        workspace_id: WorkspaceId,
        id: McpEndpointId,
        input: &McpEndpointInput,
    ) -> Result<McpEndpoint> {
        let row: McpEndpointRow = sqlx::query_as(&format!(
            "UPDATE mcp_endpoints
                SET name = $3, description = $4, script = $5, bucket_name = $6,
                    key_prefix = $7, grant_id = $8, enabled = $9, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2
             RETURNING {MCP_ENDPOINT_COLS}"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(&input.name)
        .bind(&input.description)
        .bind(&input.script)
        .bind(input.bucket_name.as_deref())
        .bind(input.key_prefix.as_deref())
        .bind(input.grant_id.map(GrantId::into_uuid))
        .bind(input.enabled)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Delete an endpoint by id, scoped to its workspace.
    pub async fn delete(&self, workspace_id: WorkspaceId, id: McpEndpointId) -> Result<()> {
        let res = sqlx::query("DELETE FROM mcp_endpoints WHERE id = $1 AND workspace_id = $2")
            .bind(id.into_uuid())
            .bind(workspace_id.into_uuid())
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
// MCP endpoint share tokens (revocable scoped tokens, SOUL §26)
// ===========================================================================

/// A minted share token for an MCP endpoint, as shown in a management listing.
/// The raw token is never stored (only its SHA-256 hash) and never leaves the
/// mint response — a listing carries only id + timestamps + revocation state.
#[derive(Clone, Debug)]
pub struct McpEndpointToken {
    /// The token row id (the handle used to revoke).
    pub id: Uuid,
    pub workspace_id: WorkspaceId,
    pub endpoint_id: McpEndpointId,
    /// When the token was minted.
    pub created_at: DateTime<Utc>,
    /// When it expires (also enforced by the signed claims).
    pub expires_at: DateTime<Utc>,
    /// When it was revoked, if it has been.
    pub revoked_at: Option<DateTime<Utc>>,
}

/// The stored columns of a `mcp_endpoint_tokens` row, projected identically
/// into a [`McpEndpointTokenRow`]. `token_hash` is deliberately excluded from
/// the general projection — it is only ever used in the `WHERE` of the
/// serve-time lookup, never read back out.
const MCP_ENDPOINT_TOKEN_COLS: &str =
    "id, workspace_id, endpoint_id, created_at, expires_at, revoked_at";

#[derive(sqlx::FromRow)]
struct McpEndpointTokenRow {
    id: Uuid,
    workspace_id: Uuid,
    endpoint_id: Uuid,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    revoked_at: Option<DateTime<Utc>>,
}

impl From<McpEndpointTokenRow> for McpEndpointToken {
    fn from(r: McpEndpointTokenRow) -> Self {
        Self {
            id: r.id,
            workspace_id: WorkspaceId::from_uuid(r.workspace_id),
            endpoint_id: McpEndpointId::from_uuid(r.endpoint_id),
            created_at: r.created_at,
            expires_at: r.expires_at,
            revoked_at: r.revoked_at,
        }
    }
}

/// CRUD for the `mcp_endpoint_tokens` table (SOUL §26). The HMAC signature on a
/// scoped endpoint token makes it unforgeable; this table makes it
/// **revocable**: the serve path (`POST /mcp/s/{token}`) requires a live row
/// (present, not revoked, not expired) in addition to a valid signature, so
/// deleting the row (or the endpoint, via cascade) kills the token immediately.
/// Every query is workspace-filtered (SOUL §18); the store only ever sees the
/// token's hash.
#[derive(Clone, Debug)]
pub struct McpEndpointTokenRepo {
    pool: PgPool,
}

impl McpEndpointTokenRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Record a freshly-minted token. `token_hash` must already be the hash of
    /// the raw token (the store never sees plaintext). A duplicate hash is a
    /// [`StoreError::Conflict`].
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        endpoint_id: McpEndpointId,
        token_hash: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<McpEndpointToken> {
        let row: McpEndpointTokenRow = sqlx::query_as(&format!(
            "INSERT INTO mcp_endpoint_tokens
                 (id, workspace_id, endpoint_id, token_hash, expires_at)
             VALUES ($1, $2, $3, $4, $5)
             RETURNING {MCP_ENDPOINT_TOKEN_COLS}"
        ))
        .bind(Uuid::new_v4())
        .bind(workspace_id.into_uuid())
        .bind(endpoint_id.into_uuid())
        .bind(token_hash)
        .bind(expires_at)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// The serve-time check: is there a **live** (not revoked, not expired)
    /// token with this hash? Returns the row, or [`StoreError::NotFound`] —
    /// indistinguishable between unknown, revoked, and expired (no probing
    /// signal, mirroring the route's `404`).
    pub async fn get_live_by_token_hash(&self, token_hash: &str) -> Result<McpEndpointToken> {
        let row: McpEndpointTokenRow = sqlx::query_as(&format!(
            "SELECT {MCP_ENDPOINT_TOKEN_COLS} FROM mcp_endpoint_tokens
             WHERE token_hash = $1 AND revoked_at IS NULL AND expires_at > CURRENT_TIMESTAMP"
        ))
        .bind(token_hash)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// List an endpoint's tokens (newest first), workspace-scoped, for the
    /// management panel. Revoked/expired rows are included so the panel can
    /// show them as dead.
    pub async fn list_by_endpoint(
        &self,
        workspace_id: WorkspaceId,
        endpoint_id: McpEndpointId,
    ) -> Result<Vec<McpEndpointToken>> {
        let rows: Vec<McpEndpointTokenRow> = sqlx::query_as(&format!(
            "SELECT {MCP_ENDPOINT_TOKEN_COLS} FROM mcp_endpoint_tokens
             WHERE workspace_id = $1 AND endpoint_id = $2
             ORDER BY created_at DESC, id ASC"
        ))
        .bind(workspace_id.into_uuid())
        .bind(endpoint_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Revoke a token by id, scoped to its workspace + endpoint (a foreign id
    /// is [`StoreError::NotFound`]). Idempotent: re-revoking an already-revoked
    /// token succeeds.
    pub async fn revoke(
        &self,
        workspace_id: WorkspaceId,
        endpoint_id: McpEndpointId,
        id: Uuid,
    ) -> Result<()> {
        let res = sqlx::query(
            "UPDATE mcp_endpoint_tokens SET revoked_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2 AND endpoint_id = $3 AND revoked_at IS NULL",
        )
        .bind(id)
        .bind(workspace_id.into_uuid())
        .bind(endpoint_id.into_uuid())
        .execute(&self.pool)
        .await
        .map_err(map)?;
        if res.rows_affected() == 0 {
            // Distinguish "already revoked" (idempotent success) from "no such
            // token" with a follow-up existence check.
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS(
                     SELECT 1 FROM mcp_endpoint_tokens
                     WHERE id = $1 AND workspace_id = $2 AND endpoint_id = $3)",
            )
            .bind(id)
            .bind(workspace_id.into_uuid())
            .bind(endpoint_id.into_uuid())
            .fetch_one(&self.pool)
            .await
            .map_err(map)?;
            if !exists {
                return Err(StoreError::NotFound);
            }
        }
        Ok(())
    }

    /// Garbage-collect tokens past their expiry. Returns the count removed.
    pub async fn delete_expired(&self) -> Result<u64> {
        let res = sqlx::query(
            "DELETE FROM mcp_endpoint_tokens WHERE expires_at <= CURRENT_TIMESTAMP",
        )
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(res.rows_affected())
    }
}

// ===========================================================================
// Computer agents (enrolled server/desktop daemons, SOUL §19/§20)
// ===========================================================================

/// The stored columns of a `computer_agents` row, projected identically into a
/// [`ComputerAgentRow`]. `token_hash` is deliberately excluded from the general
/// projection — it is only ever read by the token-lookup path.
const COMPUTER_AGENT_COLS: &str = "id, workspace_id, user_id, name, token_hash, platform, \
     capabilities, created_at, last_seen_at, revoked_at";

/// CRUD for the `computer_agents` table (SOUL §19/§20). Every query is
/// workspace-filtered (SOUL §18); `(workspace_id, name)` is UNIQUE. The enrollment
/// token is stored only as its hash (`token_hash`); the plaintext is returned to
/// the enroller exactly once at [`create`](ComputerAgentRepo::create) time by the
/// caller (this repo takes the already-hashed value).
#[derive(Clone, Debug)]
pub struct ComputerAgentRepo {
    pool: PgPool,
}

impl ComputerAgentRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Enroll a new agent owned by `user_id`. `token_hash` is the SHA-256 hash of
    /// the freshly minted token the caller hands back once. A duplicate
    /// `(workspace, name)` is a [`StoreError::Conflict`].
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        user_id: UserId,
        name: &str,
        token_hash: &str,
    ) -> Result<ComputerAgent> {
        let row: ComputerAgentRow = sqlx::query_as(&format!(
            "INSERT INTO computer_agents (id, workspace_id, user_id, name, token_hash)
             VALUES ($1, $2, $3, $4, $5)
             RETURNING {COMPUTER_AGENT_COLS}"
        ))
        .bind(ComputerAgentId::new().into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(user_id.into_uuid())
        .bind(name)
        .bind(token_hash)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Look up an **active** (non-revoked) agent by its token hash — the WS
    /// handshake path. A revoked or unknown token yields [`StoreError::NotFound`].
    pub async fn get_active_by_token_hash(&self, token_hash: &str) -> Result<ComputerAgent> {
        let row: ComputerAgentRow = sqlx::query_as(&format!(
            "SELECT {COMPUTER_AGENT_COLS} FROM computer_agents
             WHERE token_hash = $1 AND revoked_at IS NULL"
        ))
        .bind(token_hash)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch one agent by id, scoped to its workspace.
    pub async fn get(
        &self,
        workspace_id: WorkspaceId,
        id: ComputerAgentId,
    ) -> Result<ComputerAgent> {
        let row: ComputerAgentRow = sqlx::query_as(&format!(
            "SELECT {COMPUTER_AGENT_COLS} FROM computer_agents
             WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// List a workspace's agents (including revoked), most-recently-enrolled first.
    pub async fn list_by_workspace(&self, workspace_id: WorkspaceId) -> Result<Vec<ComputerAgent>> {
        let rows: Vec<ComputerAgentRow> = sqlx::query_as(&format!(
            "SELECT {COMPUTER_AGENT_COLS} FROM computer_agents
             WHERE workspace_id = $1
             ORDER BY created_at DESC, id ASC"
        ))
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(ComputerAgent::from).collect())
    }

    /// Record a live connection: refresh the announced capabilities + denormalised
    /// platform and bump `last_seen_at`. Called on connect and on each heartbeat.
    pub async fn touch_seen(
        &self,
        id: ComputerAgentId,
        capabilities: &ComputerCapabilities,
    ) -> Result<()> {
        let platform = computer_platform_to_text(capabilities.platform)?;
        sqlx::query(
            "UPDATE computer_agents
                SET capabilities = $2, platform = $3, last_seen_at = CURRENT_TIMESTAMP
             WHERE id = $1",
        )
        .bind(id.into_uuid())
        .bind(Json(capabilities))
        .bind(platform)
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(())
    }

    /// Revoke an agent (its token stops authenticating; the row is retained for
    /// audit). Idempotent — a second revoke keeps the original timestamp.
    pub async fn revoke(&self, workspace_id: WorkspaceId, id: ComputerAgentId) -> Result<()> {
        let res = sqlx::query(
            "UPDATE computer_agents
                SET revoked_at = COALESCE(revoked_at, CURRENT_TIMESTAMP)
             WHERE id = $1 AND workspace_id = $2",
        )
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .execute(&self.pool)
        .await
        .map_err(map)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// Permanently delete an agent row, scoped to its workspace.
    pub async fn delete(&self, workspace_id: WorkspaceId, id: ComputerAgentId) -> Result<()> {
        let res = sqlx::query("DELETE FROM computer_agents WHERE id = $1 AND workspace_id = $2")
            .bind(id.into_uuid())
            .bind(workspace_id.into_uuid())
            .execute(&self.pool)
            .await
            .map_err(map)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }
}
