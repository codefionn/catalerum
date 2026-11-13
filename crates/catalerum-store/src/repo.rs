//! Typed CRUD repositories over the M1 tables.
//!
//! Every repository is a thin, cloneable handle holding a [`PgPool`]. All
//! tenant queries are workspace-filtered (SOUL §6.1). Queries use the
//! **runtime** sqlx API (`query`, `query_as::<_, Row>`) — never the
//! compile-time-checked macros, since there is no database at build time.

use std::time::Duration;

use chrono::{DateTime, Utc};
// Interpolated SQL below uses fixed column lists and locally chosen clauses.
// Values supplied by callers remain bound parameters. Defined before the child
// `mod` declarations so every domain module can use it.
macro_rules! assert_sql_safe {
    ($($args:tt)*) => {
        sqlx::AssertSqlSafe(format!($($args)*))
    };
}
// Kept as a local alias to avoid obscuring the repository code with backend
// conditionals. Its concrete type is chosen at compile time in `pool`.
use crate::DbPool as PgPool;

use crate::error::StoreError;

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

mod identity;
mod orgs;
mod sessions;
mod conversations;
mod messages;
mod knowledge;
mod ui_definitions;
mod approvals;
mod connections;
mod objects;
mod events;
mod grants;
mod email;
mod jobs;
mod documents;
mod memories;
mod settings;
mod app_data;
mod boards;
mod skills;
mod agent_profiles;
mod mcp_servers;
mod terminals;
mod automations;
mod external_db;
mod mcp_endpoints;
mod computer_agents;

pub use identity::{
    BootstrapAccount, LlmleafTopologyEntry, LlmleafTopologyRepo, PasswordAccount, PasswordAuthRepo,
    UserRepo, WorkspaceRepo,
};
pub use orgs::{MembershipRepo, OrganisationRepo, OrgMembershipRepo};
pub use sessions::{LoginTokenRepo, SessionRepo};
pub use conversations::ConversationRepo;
pub use messages::{DEFAULT_MESSAGE_SEARCH_LIMIT, MessageRepo, MessageSearchHit, NewMessage};
pub use knowledge::{
    DEFAULT_LABEL_LIMIT, DEFAULT_LINK_LIMIT, DEFAULT_NOTE_LIMIT, LinkRepo, NoteRepo,
    ObjectLabelRepo,
};
pub use ui_definitions::{UiDefinitionInput, UiDefinitionRepo};
pub use approvals::{PendingApprovalRepo, PendingQuestionRepo};
pub use connections::{CalendarRepo, ConnectionRepo};
pub use objects::{
    BucketRepo, DEFAULT_OBJECT_LIMIT, DEFAULT_OBJECT_SEARCH_LIMIT, ObjectRepo, ObjectTextHit,
    UpsertObject,
};
pub use events::{DateRange, DEFAULT_EVENT_LIMIT, EventPatch, EventRepo, UpsertEvent};
pub use grants::GrantRepo;
pub use email::{EmailRepo, MailboxRepo};
pub use jobs::JobQueueRepo;
pub use documents::{ChunkRepo, DocumentRepo, NewChunk};
pub use memories::MemoryRepo;
pub use settings::{LlmSettingsRepo, ProfileRepo, SearchSettingsRepo, StorageSettingsRepo};
pub use app_data::{AppDataEntry, AppDataRepo, MAX_APP_DATA_KEYS_PER_APP, MAX_APP_DATA_VALUE_BYTES};
pub use boards::{BoardRepo, DEFAULT_COLUMNS, TaskRepo};
pub use skills::{NewSkill, SkillRepo};
pub use agent_profiles::{AgentProfileRepo, NewAgentProfile};
pub use mcp_servers::{McpServerRepo, NewMcpServerDef};
pub use terminals::{
    NewTerminalSession, NewWorkspaceSandbox, PodHeartbeatRepo, TerminalSessionRepo,
    WorkspaceSandboxRepo,
};
pub use automations::{AutomationRepo, AutomationRunRepo, NewAutomation};
pub use external_db::{ExternalDbMigration, ExternalDbMigrationRepo};
pub use mcp_endpoints::{McpEndpointInput, McpEndpointRepo, McpEndpointToken, McpEndpointTokenRepo};
pub use computer_agents::ComputerAgentRepo;
