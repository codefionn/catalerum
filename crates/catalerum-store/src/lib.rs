//! catalerum-store — Postgres source of truth via sqlx: migrations and typed
//! repositories. Every tenant row carries a `workspace_id` and all queries are
//! workspace-filtered (SOUL §6.1).
//!
//! # Layout
//! - [`pool`] — [`connect`]/[`connect_with`] pool constructors and the embedded
//!   [`MIGRATOR`] (`migrate`/`connect_and_migrate`).
//! - [`rows`] — `sqlx::FromRow` row mirrors and their conversions to
//!   [`catalerum_core`] domain types.
//! - [`repo`] — typed CRUD repositories.
//! - [`Store`] — an aggregate facade owning one [`sqlx::PgPool`] and handing out
//!   every repository.
//!
//! All queries use the **runtime** sqlx API (`query`, `query_as::<_, Row>`) with
//! `#[derive(sqlx::FromRow)]` — never the compile-time-checked macros — because
//! there is no database available at build time. `sqlx::migrate!` is fine: it
//! only reads the migrations directory.
//!
//! ```no_run
//! # async fn run() -> catalerum_store::Result<()> {
//! let store = catalerum_store::Store::connect("postgres://localhost/catalerum").await?;
//! let ws = store.workspaces().create("Acme", "acme").await?;
//! let convo = store
//!     .conversations()
//!     .create(ws.id, Some("hello"), catalerum_core::model::Origin::Web)
//!     .await?;
//! let _ = convo;
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]

pub mod error;
pub mod pool;
pub mod repo;
pub mod rows;
pub mod secret;
pub mod sql;

pub use error::{Result, StoreError};
pub use pool::{
    connect, connect_and_migrate, connect_external, connect_with, migrate, ping_external_pool,
    ping_pool, ActiveBackend, BackendPool, DbPool, PgConnectSpec, PoolConfig, PostgresBackend,
    PostgresConnectionConfig, RepositoryBackend, SqliteBackend, MIGRATOR,
};
pub use repo::{
    AgentProfileRepo, AppDataEntry, AppDataRepo, AutomationRepo, AutomationRunRepo, BoardRepo,
    BootstrapAccount, BucketRepo, CalendarRepo, ChunkRepo, ComputerAgentRepo, ConnectionRepo,
    ConversationRepo, DateRange, DocumentRepo, EmailRepo, EventPatch, EventRepo,
    ExternalDbMigration, ExternalDbMigrationRepo, GrantRepo, JobQueueRepo, LinkRepo,
    LlmSettingsRepo, LlmleafTopologyEntry, LlmleafTopologyRepo, LoginTokenRepo, MailboxRepo,
    McpEndpointInput, McpEndpointRepo, McpEndpointToken, McpEndpointTokenRepo, McpServerRepo,
    MembershipRepo, MemoryRepo, MessageRepo,
    MessageSearchHit, NewAgentProfile, NewAutomation, NewChunk, NewMcpServerDef, NewMessage,
    NewSkill, NewTerminalSession, NewWorkspaceSandbox, NoteRepo, ObjectLabelRepo, ObjectRepo,
    ObjectTextHit, OrgMembershipRepo, OrganisationRepo, PasswordAccount, PasswordAuthRepo,
    PendingApprovalRepo, PendingQuestionRepo, PodHeartbeatRepo, ProfileRepo, SearchSettingsRepo,
    SessionRepo, SkillRepo, StorageSettingsRepo, TaskRepo, TerminalSessionRepo, UiDefinitionInput,
    UiDefinitionRepo, UpsertEvent, UpsertObject, UserRepo, WorkspaceRepo, WorkspaceSandboxRepo,
    DEFAULT_COLUMNS, DEFAULT_EVENT_LIMIT, DEFAULT_LABEL_LIMIT, DEFAULT_LINK_LIMIT,
    DEFAULT_MESSAGE_SEARCH_LIMIT, DEFAULT_NOTE_LIMIT, DEFAULT_OBJECT_LIMIT,
    DEFAULT_OBJECT_SEARCH_LIMIT, DEFAULT_ORGANISATION_SLUG, MAX_APP_DATA_KEYS_PER_APP,
    MAX_APP_DATA_VALUE_BYTES,
};
pub use rows::{
    creation_policy_to_text, org_role_to_text, source_from_parts, source_to_parts, AgentProfileRow,
    AppDataRow, AutomationRow, AutomationRunRow, AutomationStepRow, BoardRow, BucketRow,
    CalendarRow, ChunkRow, ColumnRow, ComputerAgent, ComputerAgentRow, ConnectionRow, DocumentRow,
    EmailRow, EventRow, JobRow, JobStatus, LinkRow, LoginToken, MailboxRow, MemoryRow, NoteRow,
    ObjectLabelRow, ObjectRow, OrgMembershipRow, OrganisationRow, ProfileRow, SearchSettingsRow,
    Session, SkillRow, StorageSettingsRow, TaskRow, TerminalSessionRow, UiDefinitionRow,
    WorkspaceSandboxRow,
};
pub use secret::{SecretStore, MASTER_KEY_LEN};
pub use sql::{
    run_ddl_batch as sql_run_ddl_batch, run_read as sql_run_read, run_sql_script as sql_run_script,
    run_write as sql_run_write,
};

// External workspace databases are PostgreSQL even in the all-in-one build.
pub use sqlx::PgPool;

/// Embedded migrations directory path (relative to the crate root). The actual
/// migration set is bundled at compile time into [`MIGRATOR`].
pub const MIGRATIONS_DIR: &str = "migrations";

/// Aggregate handle over a single Postgres [`PgPool`]. Cloning is cheap (the
/// pool is internally `Arc`-shared); share one `Store` across the application.
#[derive(Clone, Debug)]
pub struct Store {
    pool: DbPool,
}

impl Store {
    /// Wrap an existing pool.
    #[must_use]
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// Connect to `database_url` and apply embedded migrations.
    pub async fn connect(database_url: &str) -> Result<Self> {
        let pool = connect_and_migrate(database_url).await?;
        Ok(Self { pool })
    }

    /// Connect to `database_url` with explicit pool options and apply migrations.
    pub async fn connect_with(database_url: &str, config: &PoolConfig) -> Result<Self> {
        let pool = connect_with(database_url, config).await?;
        migrate(&pool).await?;
        Ok(Self { pool })
    }

    /// Borrow the underlying pool (for ad-hoc queries or other crates).
    #[must_use]
    pub fn pool(&self) -> &DbPool {
        &self.pool
    }

    /// Cheap liveness probe: `SELECT 1` against the pool. `Ok(())` means the
    /// database is reachable; an `Err` means it is down/unreachable. Powers the
    /// `/status` health surface (SOUL §12).
    pub async fn ping(&self) -> Result<()> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    /// Run embedded migrations against this store's pool.
    pub async fn migrate(&self) -> Result<()> {
        migrate(&self.pool).await
    }

    /// Workspaces repository.
    #[must_use]
    pub fn workspaces(&self) -> WorkspaceRepo {
        WorkspaceRepo::new(self.pool.clone())
    }

    /// Users repository.
    #[must_use]
    pub fn users(&self) -> UserRepo {
        UserRepo::new(self.pool.clone())
    }

    /// Local-password and first-boot repository (disabled by configuration in
    /// the normal distributed deployment).
    #[must_use]
    pub fn password_auth(&self) -> PasswordAuthRepo {
        PasswordAuthRepo::new(self.pool.clone())
    }

    #[must_use]
    pub fn llmleaf_topology(&self) -> LlmleafTopologyRepo {
        LlmleafTopologyRepo::new(self.pool.clone())
    }

    /// Memberships repository.
    #[must_use]
    pub fn memberships(&self) -> MembershipRepo {
        MembershipRepo::new(self.pool.clone())
    }

    /// Organisations repository — the administrative grouping above workspaces
    /// (SOUL §18).
    #[must_use]
    pub fn organisations(&self) -> OrganisationRepo {
        OrganisationRepo::new(self.pool.clone())
    }

    /// Organisation-memberships repository — organisation ⇄ user administrative
    /// roles (SOUL §18).
    #[must_use]
    pub fn org_memberships(&self) -> OrgMembershipRepo {
        OrgMembershipRepo::new(self.pool.clone())
    }

    /// Sessions repository.
    #[must_use]
    pub fn sessions(&self) -> SessionRepo {
        SessionRepo::new(self.pool.clone())
    }

    /// One-time login tokens repository (dev magic-link, SOUL §18).
    #[must_use]
    pub fn login_tokens(&self) -> LoginTokenRepo {
        LoginTokenRepo::new(self.pool.clone())
    }

    /// Conversations repository.
    #[must_use]
    pub fn conversations(&self) -> ConversationRepo {
        ConversationRepo::new(self.pool.clone())
    }

    /// Markdown notes repository (SOUL §21).
    #[must_use]
    pub fn notes(&self) -> NoteRepo {
        NoteRepo::new(self.pool.clone())
    }

    /// Links repository — relationships between objects (SOUL §5/§6.3).
    #[must_use]
    pub fn links(&self) -> LinkRepo {
        LinkRepo::new(self.pool.clone())
    }

    /// Emerged-UI definitions repository (AI-authored declarative UIs).
    #[must_use]
    pub fn ui_definitions(&self) -> UiDefinitionRepo {
        UiDefinitionRepo::new(self.pool.clone())
    }

    /// Messages repository.
    #[must_use]
    pub fn messages(&self) -> MessageRepo {
        MessageRepo::new(self.pool.clone())
    }

    /// Provider connections repository (calendar / storage / channel, SOUL §8).
    #[must_use]
    pub fn connections(&self) -> ConnectionRepo {
        ConnectionRepo::new(self.pool.clone())
    }

    /// Calendars repository (SOUL §8).
    #[must_use]
    pub fn calendars(&self) -> CalendarRepo {
        CalendarRepo::new(self.pool.clone())
    }

    /// Buckets repository — storage containers on a connection (SOUL §9).
    #[must_use]
    pub fn buckets(&self) -> BucketRepo {
        BucketRepo::new(self.pool.clone())
    }

    /// Objects repository — catalogued metadata for stored objects (SOUL §9/§10).
    #[must_use]
    pub fn objects(&self) -> ObjectRepo {
        ObjectRepo::new(self.pool.clone())
    }

    /// Object-labels repository — user/agent tags on stored files & directories
    /// (SOUL §9).
    #[must_use]
    pub fn object_labels(&self) -> ObjectLabelRepo {
        ObjectLabelRepo::new(self.pool.clone())
    }

    /// Calendar events repository (SOUL §8/§10).
    #[must_use]
    pub fn events(&self) -> EventRepo {
        EventRepo::new(self.pool.clone())
    }

    /// Mailboxes repository — email folders on a connection (SOUL §28).
    #[must_use]
    pub fn mailboxes(&self) -> MailboxRepo {
        MailboxRepo::new(self.pool.clone())
    }

    /// Grants repository — named capability bundles (SOUL §19).
    #[must_use]
    pub fn grants(&self) -> GrantRepo {
        GrantRepo::new(self.pool.clone())
    }

    /// Emails repository — normalized messages (SOUL §28/§10).
    #[must_use]
    pub fn emails(&self) -> EmailRepo {
        EmailRepo::new(self.pool.clone())
    }

    /// Durable job-queue repository (SOUL §6.2).
    #[must_use]
    pub fn job_queue(&self) -> JobQueueRepo {
        JobQueueRepo::new(self.pool.clone())
    }

    /// Documents repository — extracted source text, the chunk/embed unit
    /// (SOUL §6.4/§10).
    #[must_use]
    pub fn documents(&self) -> DocumentRepo {
        DocumentRepo::new(self.pool.clone())
    }

    /// Chunks repository — the embedded slices of a document (SOUL §6.4).
    #[must_use]
    pub fn chunks(&self) -> ChunkRepo {
        ChunkRepo::new(self.pool.clone())
    }

    /// Memories repository — durable curated personalization facts (SOUL §22).
    #[must_use]
    pub fn memories(&self) -> MemoryRepo {
        MemoryRepo::new(self.pool.clone())
    }

    /// Profiles repository — per-user structured personalization (SOUL §22).
    #[must_use]
    pub fn profiles(&self) -> ProfileRepo {
        ProfileRepo::new(self.pool.clone())
    }

    /// LLM settings repository — per-user model/voice overrides of the `[llm]`
    /// config defaults (SOUL §7/§13).
    #[must_use]
    pub fn llm_settings(&self) -> LlmSettingsRepo {
        LlmSettingsRepo::new(self.pool.clone())
    }

    /// Search settings repository — per-user default-provider override of the
    /// `[search]` config default (SOUL §7/§13).
    #[must_use]
    pub fn search_settings(&self) -> SearchSettingsRepo {
        SearchSettingsRepo::new(self.pool.clone())
    }

    /// Storage settings repository — per-user default-store override of the
    /// `[storage]` config default (SOUL §7/§9/§13).
    #[must_use]
    pub fn storage_settings(&self) -> StorageSettingsRepo {
        StorageSettingsRepo::new(self.pool.clone())
    }

    /// Per-App durable key/value store — `(app, key) → JSONB` an emerged App's
    /// handlers persist and present (SOUL §12/§29).
    #[must_use]
    pub fn app_data(&self) -> AppDataRepo {
        AppDataRepo::new(self.pool.clone())
    }

    /// Boards repository — Kanban boards + columns (SOUL §24).
    #[must_use]
    pub fn boards(&self) -> BoardRepo {
        BoardRepo::new(self.pool.clone())
    }

    /// Tasks repository — Kanban tasks (SOUL §24).
    #[must_use]
    pub fn tasks(&self) -> TaskRepo {
        TaskRepo::new(self.pool.clone())
    }

    /// Skills repository — markdown-defined reusable capability bundles (SOUL §23).
    #[must_use]
    pub fn skills(&self) -> SkillRepo {
        SkillRepo::new(self.pool.clone())
    }

    /// MCP endpoints repository — user-authored, Boa-scripted scoped MCP endpoints
    /// (SOUL §26).
    #[must_use]
    pub fn mcp_endpoints(&self) -> McpEndpointRepo {
        McpEndpointRepo::new(self.pool.clone())
    }

    /// MCP endpoint share-token repository — the revocable, hash-only record
    /// behind `POST /mcp/s/{token}` (SOUL §26).
    #[must_use]
    pub fn mcp_endpoint_tokens(&self) -> McpEndpointTokenRepo {
        McpEndpointTokenRepo::new(self.pool.clone())
    }

    /// Agent profiles repository — durable, channel-bindable scoped-agent
    /// configurations (SOUL §19/§25).
    #[must_use]
    pub fn agent_profiles(&self) -> AgentProfileRepo {
        AgentProfileRepo::new(self.pool.clone())
    }

    /// External MCP servers repository — runtime-managed client connections (SOUL §26).
    #[must_use]
    pub fn mcp_servers(&self) -> McpServerRepo {
        McpServerRepo::new(self.pool.clone())
    }

    /// Computer agents repository — enrolled server/desktop daemons the LLM drives
    /// over an authenticated WebSocket (SOUL §19/§20).
    #[must_use]
    pub fn computer_agents(&self) -> ComputerAgentRepo {
        ComputerAgentRepo::new(self.pool.clone())
    }

    /// Terminal sessions repository — active + history (SOUL §20).
    #[must_use]
    pub fn terminal_sessions(&self) -> TerminalSessionRepo {
        TerminalSessionRepo::new(self.pool.clone())
    }

    /// Per-workspace sandboxes repository — one long-lived container/Pod per
    /// workspace (SOUL §20).
    #[must_use]
    pub fn workspace_sandboxes(&self) -> WorkspaceSandboxRepo {
        WorkspaceSandboxRepo::new(self.pool.clone())
    }

    /// Pod-heartbeat repository — per-process liveness so the stale-pod reclaim
    /// sweeps self-heal a permanently-dead pod's terminal/sandbox rows (SOUL §20/§16 M7).
    #[must_use]
    pub fn pod_heartbeats(&self) -> PodHeartbeatRepo {
        PodHeartbeatRepo::new(self.pool.clone())
    }

    /// Automations repository — durable trigger→condition→action definitions (SOUL §11).
    #[must_use]
    pub fn automations(&self) -> AutomationRepo {
        AutomationRepo::new(self.pool.clone())
    }

    /// Automation run/step state repository — durable execution audit (SOUL §11).
    #[must_use]
    pub fn automation_runs(&self) -> AutomationRunRepo {
        AutomationRunRepo::new(self.pool.clone())
    }

    /// External-DB manual-migration repository (SOUL §11) — ordered SQL migration
    /// scripts + the applied ledger for external Postgres connections.
    #[must_use]
    pub fn external_db_migrations(&self) -> ExternalDbMigrationRepo {
        ExternalDbMigrationRepo::new(self.pool.clone())
    }

    /// Pending `ask_user` question forms (SOUL §7/§12) — persisted so an
    /// interactive question survives a reload/reconnect.
    #[must_use]
    pub fn pending_questions(&self) -> PendingQuestionRepo {
        PendingQuestionRepo::new(self.pool.clone())
    }

    /// Deferred tool-call approvals (SOUL §7/§12/§19) — a guard-gated call held
    /// until the user Approves/Rejects; durable so the prompt survives a reload /
    /// reconnect / restart.
    #[must_use]
    pub fn pending_approvals(&self) -> PendingApprovalRepo {
        PendingApprovalRepo::new(self.pool.clone())
    }

    /// Encrypted secret store over this pool, keyed by `master_key` (SOUL §13).
    /// Used to store external-provider credentials encrypted at rest. Fails if
    /// `master_key` is not [`MASTER_KEY_LEN`] bytes.
    pub fn secret_store(&self, master_key: &[u8]) -> Result<SecretStore> {
        SecretStore::new(self.pool.clone(), master_key)
    }
}
