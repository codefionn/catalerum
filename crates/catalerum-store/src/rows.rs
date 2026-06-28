//! `sqlx::FromRow` row structs and their conversions to/from
//! [`catalerum_core`] domain types.
//!
//! Enums are stored as lowercase `TEXT` (matching the core `snake_case` serde
//! representations) and parsed back via small helpers; structured fields
//! (`tool_calls`) ride in `JSONB`.

use catalerum_core::ask::{Answer, Question};
use catalerum_core::capability::{Capability, Constraints};
use catalerum_core::computer::{ComputerCapabilities, ComputerPlatform};
use catalerum_core::{
    id::{
        AgentId, AgentProfileId, AutomationId, AutomationRunId, AutomationStepId, BoardId,
        BucketId, CalendarId, ChunkId, ColumnId, ComputerAgentId, ConnectionId, ConversationId,
        DocumentId, EmailId, EventId, GrantId, LinkId, MailboxId, McpEndpointId, McpServerId,
        MemoryId, MessageId, NoteId, ObjectId, ObjectLabelId, OrganisationId, PendingApprovalId,
        PendingQuestionId, SkillId, TaskId, TerminalSessionId, UiDefinitionId, UserId, WorkspaceId,
    },
    model::{
        AgentProfile, ApprovalDecision, Attachment, Author, Automation, AutomationRun,
        AutomationStep, Board, Bucket, Calendar, Chunk, Code, Column, Connection, ConnectionKind,
        Conversation, CreationPolicy, Cursor, Document, Email, EmailAddress, EntityRef, Event,
        ExecutorKind, Grant, Link, LlmSettings, Mailbox, Map, McpAuthSpec, McpEndpoint,
        McpServerDef, Membership, Memory, MemoryScope, Message, MessageRole, Note, ObjectLabel,
        OrgMembership, OrgRole, Organisation, Origin, PendingApproval, PendingQuestion, Profile,
        Role, RunStatus, SandboxState, SearchSettings, Skill, SkillInvocation, SourceRef,
        StepStatus, StorageSettings, StoredObject, Subject, Task, TaskStatus, TerminalSession,
        TerminalSessionStatus, ToolCall, ToolGuard, UiDefinition, User, Workspace,
        WorkspaceSandboxRecord,
    },
    model_ui::UiSpec,
};
use chrono::{DateTime, Utc};
use sqlx::types::Json;
use uuid::Uuid;

use crate::error::StoreError;

// ---------------------------------------------------------------------------
// Enum <-> TEXT helpers
// ---------------------------------------------------------------------------

/// Serialize a `snake_case` serde enum to its wire token (without quotes).
fn enum_to_text<T: serde::Serialize>(value: &T) -> Result<String, StoreError> {
    match serde_json::to_value(value).map_err(StoreError::decode)? {
        serde_json::Value::String(s) => Ok(s),
        other => Err(StoreError::decode(format!(
            "expected string enum, got {other}"
        ))),
    }
}

/// Parse a `snake_case` serde enum from its wire token.
fn enum_from_text<T: serde::de::DeserializeOwned>(text: &str) -> Result<T, StoreError> {
    serde_json::from_value(serde_json::Value::String(text.to_owned())).map_err(StoreError::decode)
}

// ---------------------------------------------------------------------------
// workspaces
// ---------------------------------------------------------------------------

/// Row mirror of the `workspaces` table.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct WorkspaceRow {
    pub id: Uuid,
    pub organisation_id: Uuid,
    pub name: String,
    pub slug: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Soft-archive timestamp, or `NULL` while active (SOUL §18).
    pub archived_at: Option<DateTime<Utc>>,
}

impl From<WorkspaceRow> for Workspace {
    fn from(r: WorkspaceRow) -> Self {
        Workspace {
            id: WorkspaceId::from_uuid(r.id),
            organisation_id: OrganisationId::from_uuid(r.organisation_id),
            name: r.name,
            slug: r.slug,
            archived_at: r.archived_at,
        }
    }
}

// ---------------------------------------------------------------------------
// organisations
// ---------------------------------------------------------------------------

/// Row mirror of the `organisations` table (SOUL §18).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct OrganisationRow {
    pub id: Uuid,
    pub name: String,
    pub slug: String,
    pub workspace_creation: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl TryFrom<OrganisationRow> for Organisation {
    type Error = StoreError;

    fn try_from(r: OrganisationRow) -> Result<Self, Self::Error> {
        Ok(Organisation {
            id: OrganisationId::from_uuid(r.id),
            name: r.name,
            slug: r.slug,
            workspace_creation: enum_from_text::<CreationPolicy>(&r.workspace_creation)?,
        })
    }
}

/// Encode a [`CreationPolicy`] to its stored `TEXT` token.
pub fn creation_policy_to_text(policy: CreationPolicy) -> Result<String, StoreError> {
    enum_to_text(&policy)
}

// ---------------------------------------------------------------------------
// org_memberships
// ---------------------------------------------------------------------------

/// Row mirror of the `org_memberships` table (SOUL §18).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct OrgMembershipRow {
    pub organisation_id: Uuid,
    pub user_id: Uuid,
    pub role: String,
    pub created_at: DateTime<Utc>,
}

impl TryFrom<OrgMembershipRow> for OrgMembership {
    type Error = StoreError;

    fn try_from(r: OrgMembershipRow) -> Result<Self, Self::Error> {
        Ok(OrgMembership {
            organisation_id: OrganisationId::from_uuid(r.organisation_id),
            user_id: UserId::from_uuid(r.user_id),
            role: enum_from_text::<OrgRole>(&r.role)?,
        })
    }
}

/// Encode an [`OrgRole`] to its stored `TEXT` token.
pub fn org_role_to_text(role: OrgRole) -> Result<String, StoreError> {
    enum_to_text(&role)
}

// ---------------------------------------------------------------------------
// users
// ---------------------------------------------------------------------------

/// Row mirror of the `users` table.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UserRow {
    pub id: Uuid,
    pub email: String,
    pub display_name: String,
    pub sso_issuer: Option<String>,
    pub sso_subject: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<UserRow> for User {
    fn from(r: UserRow) -> Self {
        let sso_subject = match (r.sso_issuer, r.sso_subject) {
            (Some(issuer), Some(subject)) => Some(Subject { issuer, subject }),
            _ => None,
        };
        User {
            id: UserId::from_uuid(r.id),
            email: r.email,
            display_name: r.display_name,
            sso_subject,
        }
    }
}

// ---------------------------------------------------------------------------
// memberships
// ---------------------------------------------------------------------------

/// Row mirror of the `memberships` table.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MembershipRow {
    pub workspace_id: Uuid,
    pub user_id: Uuid,
    pub role: String,
    pub created_at: DateTime<Utc>,
}

impl TryFrom<MembershipRow> for Membership {
    type Error = StoreError;

    fn try_from(r: MembershipRow) -> Result<Self, Self::Error> {
        Ok(Membership {
            workspace_id: WorkspaceId::from_uuid(r.workspace_id),
            user_id: UserId::from_uuid(r.user_id),
            role: enum_from_text::<Role>(&r.role)?,
        })
    }
}

/// Encode a [`Role`] to its stored `TEXT` token.
pub fn role_to_text(role: Role) -> Result<String, StoreError> {
    enum_to_text(&role)
}

/// Encode a [`ComputerPlatform`] to its stored `TEXT` token (the denormalised
/// `computer_agents.platform` column).
pub fn computer_platform_to_text(p: ComputerPlatform) -> Result<String, StoreError> {
    enum_to_text(&p)
}

// ---------------------------------------------------------------------------
// sessions (store-only; no core type)
// ---------------------------------------------------------------------------

/// An opaque server-side authentication session. Store-only — there is no
/// `catalerum-core` analogue. `token_hash` is a hash of the bearer/cookie
/// token, never the raw token.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Session {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub user_id: Uuid,
    pub token_hash: String,
    /// The named §19 grant this token is scoped to, or `NULL` for a role-derived
    /// session (SOUL §19/§26). A same-workspace composite FK to `grants` cascade-
    /// revokes the session when the grant is deleted.
    pub grant_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

impl Session {
    /// The owning workspace as a typed id.
    #[must_use]
    pub fn workspace_id(&self) -> WorkspaceId {
        WorkspaceId::from_uuid(self.workspace_id)
    }

    /// The authenticated user as a typed id.
    #[must_use]
    pub fn user_id(&self) -> UserId {
        UserId::from_uuid(self.user_id)
    }

    /// The grant this token is scoped to as a typed id, if any (SOUL §19).
    #[must_use]
    pub fn grant_id(&self) -> Option<catalerum_core::GrantId> {
        self.grant_id.map(catalerum_core::GrantId::from_uuid)
    }
}

// ---------------------------------------------------------------------------
// login_tokens (store-only; no core type)
// ---------------------------------------------------------------------------

/// A one-time login token (dev magic-link, SOUL §18). Store-only — there is no
/// `catalerum-core` analogue. `token_hash` is a hash of the raw token handed to
/// the caller; the store never sees the plaintext token. The row is consumed
/// exactly once (`consumed_at` flips atomically on redemption).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct LoginToken {
    pub token_hash: String,
    pub workspace_id: Uuid,
    pub user_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    /// When the token was redeemed; `None` while still usable.
    pub consumed_at: Option<DateTime<Utc>>,
}

impl LoginToken {
    /// The owning workspace as a typed id.
    #[must_use]
    pub fn workspace_id(&self) -> WorkspaceId {
        WorkspaceId::from_uuid(self.workspace_id)
    }

    /// The target user as a typed id.
    #[must_use]
    pub fn user_id(&self) -> UserId {
        UserId::from_uuid(self.user_id)
    }
}

// ---------------------------------------------------------------------------
// computer_agents (store-only; capabilities from catalerum-core)
// ---------------------------------------------------------------------------

/// An enrolled **computer agent** — a daemon on a server/desktop that serves the
/// LLM's `computer_*` operations over an authenticated WebSocket (SOUL §19/§20).
/// Store-only domain type (there is no `Author`-style core analogue); its wire
/// [`ComputerCapabilities`] snapshot comes from `catalerum-core`. `token_hash` is
/// a SHA-256 hash of the enrollment token the enroller received once; the store
/// never sees the plaintext.
#[derive(Debug, Clone)]
pub struct ComputerAgent {
    pub id: ComputerAgentId,
    pub workspace_id: WorkspaceId,
    pub user_id: UserId,
    pub name: String,
    /// Denormalised platform token (out of `capabilities`) for cheap listing.
    pub platform: Option<ComputerPlatform>,
    /// The machine's last-announced capabilities, or `None` before first connect.
    pub capabilities: Option<ComputerCapabilities>,
    pub created_at: DateTime<Utc>,
    /// Bumped while a connection is live; `None` if never connected.
    pub last_seen_at: Option<DateTime<Utc>>,
    /// When the agent was revoked; `None` while still usable.
    pub revoked_at: Option<DateTime<Utc>>,
}

impl ComputerAgent {
    /// Whether this agent's token is still valid (not revoked).
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.revoked_at.is_none()
    }
}

/// Row mirror of the `computer_agents` table.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ComputerAgentRow {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub user_id: Uuid,
    pub name: String,
    pub token_hash: String,
    pub platform: Option<String>,
    pub capabilities: Option<Json<ComputerCapabilities>>,
    pub created_at: DateTime<Utc>,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
}

impl From<ComputerAgentRow> for ComputerAgent {
    fn from(r: ComputerAgentRow) -> Self {
        ComputerAgent {
            id: ComputerAgentId::from_uuid(r.id),
            workspace_id: WorkspaceId::from_uuid(r.workspace_id),
            user_id: UserId::from_uuid(r.user_id),
            name: r.name,
            platform: r
                .platform
                .as_deref()
                .and_then(|p| enum_from_text::<ComputerPlatform>(p).ok()),
            capabilities: r.capabilities.map(|j| j.0),
            created_at: r.created_at,
            last_seen_at: r.last_seen_at,
            revoked_at: r.revoked_at,
        }
    }
}

// ---------------------------------------------------------------------------
// conversations
// ---------------------------------------------------------------------------

/// Row mirror of the `conversations` table.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ConversationRow {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub title: Option<String>,
    pub tags: Json<Vec<String>>,
    pub title_manual: bool,
    pub origin: String,
    pub agent_profile_id: Option<Uuid>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub summary: Option<String>,
    pub summary_upto: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl TryFrom<ConversationRow> for Conversation {
    type Error = StoreError;

    fn try_from(r: ConversationRow) -> Result<Self, Self::Error> {
        Ok(Conversation {
            id: ConversationId::from_uuid(r.id),
            workspace_id: WorkspaceId::from_uuid(r.workspace_id),
            title: r.title,
            tags: r.tags.0,
            title_manual: r.title_manual,
            origin: enum_from_text::<Origin>(&r.origin)?,
            agent_profile_id: r.agent_profile_id.map(AgentProfileId::from_uuid),
            model: r.model,
            reasoning_effort: r.reasoning_effort,
            summary: r.summary,
            summary_upto: r.summary_upto.map(MessageId::from_uuid),
            created_at: r.created_at,
        })
    }
}

/// Encode an [`Origin`] to its stored `TEXT` token.
pub fn origin_to_text(origin: Origin) -> Result<String, StoreError> {
    enum_to_text(&origin)
}

// ---------------------------------------------------------------------------
// messages
// ---------------------------------------------------------------------------

/// Row mirror of the `messages` table.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MessageRow {
    pub id: Uuid,
    pub conversation_id: Uuid,
    pub role: String,
    pub content: String,
    /// User-turn file/image references (SOUL §9/§12), JSONB array (`[]` default).
    pub attachments: Json<Vec<Attachment>>,
    /// A user turn's `/<skill>` invocation snapshot (SOUL §12/§23), nullable JSONB.
    pub skill: Option<Json<SkillInvocation>>,
    pub tool_calls: Json<Vec<ToolCall>>,
    pub tool_call_id: Option<String>,
    pub tool_is_error: bool,
    pub tool_duration_ms: Option<i64>,
    /// Per-turn token + cost accounting (the final assistant message of an
    /// exchange); `NULL` on every other row. `total_tokens` doubles as the
    /// "usage was recorded" marker — see [`MessageRow`]'s `TryFrom`.
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
    pub cached_tokens: Option<i64>,
    pub cache_creation_tokens: Option<i64>,
    pub cost_usd: Option<f64>,
    pub created_at: DateTime<Utc>,
}

/// A `messages` row joined to its conversation's `title` — for content search,
/// where each hit needs the thread it belongs to (the bare message carries only
/// `conversation_id`).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MessageSearchRow {
    pub id: Uuid,
    pub conversation_id: Uuid,
    pub role: String,
    pub content: String,
    pub tool_calls: Json<Vec<ToolCall>>,
    pub tool_call_id: Option<String>,
    pub tool_is_error: bool,
    pub tool_duration_ms: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub conversation_title: Option<String>,
}

impl MessageSearchRow {
    /// Split into the message-row part and the conversation title.
    #[must_use]
    pub fn split(self) -> (MessageRow, Option<String>) {
        (
            MessageRow {
                id: self.id,
                conversation_id: self.conversation_id,
                role: self.role,
                content: self.content,
                // Content search doesn't project attachments — a hit is a preview.
                attachments: Json(Vec::new()),
                // …nor the skill snapshot, for the same reason.
                skill: None,
                tool_calls: self.tool_calls,
                tool_call_id: self.tool_call_id,
                tool_is_error: self.tool_is_error,
                tool_duration_ms: self.tool_duration_ms,
                // Content search doesn't project usage — a hit is a preview, not a
                // transcript replay; leave it unrecorded.
                prompt_tokens: None,
                completion_tokens: None,
                total_tokens: None,
                cached_tokens: None,
                cache_creation_tokens: None,
                cost_usd: None,
                created_at: self.created_at,
            },
            self.conversation_title,
        )
    }
}

impl TryFrom<MessageRow> for Message {
    type Error = StoreError;

    fn try_from(r: MessageRow) -> Result<Self, Self::Error> {
        // Reconstruct the per-turn usage iff it was recorded. The counts are
        // always written together (or all NULL), so `total_tokens` being present
        // is the precise marker; legacy / non-final rows leave it NULL → `None`.
        // Stored as BIGINT but the counts are u32 on the wire, so clamp the cast.
        let usage = r
            .total_tokens
            .is_some()
            .then(|| catalerum_core::stream::Usage {
                prompt_tokens: row_count_to_u32(r.prompt_tokens),
                completion_tokens: row_count_to_u32(r.completion_tokens),
                total_tokens: row_count_to_u32(r.total_tokens),
                cost_usd: r.cost_usd,
                cached_tokens: row_count_to_u32(r.cached_tokens),
                cache_creation_tokens: row_count_to_u32(r.cache_creation_tokens),
            });
        Ok(Message {
            id: MessageId::from_uuid(r.id),
            conversation_id: ConversationId::from_uuid(r.conversation_id),
            role: enum_from_text::<MessageRole>(&r.role)?,
            content: r.content,
            attachments: r.attachments.0,
            skill: r.skill.map(|j| j.0),
            tool_calls: r.tool_calls.0,
            tool_call_id: r.tool_call_id,
            tool_is_error: r.tool_is_error,
            tool_duration_ms: r.tool_duration_ms,
            usage,
            created_at: r.created_at,
        })
    }
}

/// Flatten a nullable stored token count (`BIGINT`) to the `u32` the wire uses,
/// treating `NULL`/out-of-range as `0`. The counts are always non-negative and
/// far below `u32::MAX`, so the clamp only guards against corruption.
fn row_count_to_u32(v: Option<i64>) -> u32 {
    v.and_then(|n| u32::try_from(n).ok()).unwrap_or(0)
}

/// Encode a [`MessageRole`] to its stored `TEXT` token.
pub fn message_role_to_text(role: MessageRole) -> Result<String, StoreError> {
    enum_to_text(&role)
}

// ---------------------------------------------------------------------------
// notes
// ---------------------------------------------------------------------------

/// Split a core [`Author`] into its stored `(author_kind, author_id)` columns.
/// The discriminator is `user` / `agent`; the id points at the matching table.
#[must_use]
pub fn author_to_parts(author: Author) -> (&'static str, Uuid) {
    match author {
        Author::User { id } => ("user", id.into_uuid()),
        Author::Agent { id } => ("agent", id.into_uuid()),
    }
}

/// Reassemble a core [`Author`] from its stored `(author_kind, author_id)`
/// columns, or [`StoreError::Decode`] on an unknown discriminator.
pub fn author_from_parts(kind: &str, id: Uuid) -> Result<Author, StoreError> {
    match kind {
        "user" => Ok(Author::User {
            id: UserId::from_uuid(id),
        }),
        "agent" => Ok(Author::Agent {
            id: AgentId::from_uuid(id),
        }),
        other => Err(StoreError::decode(format!("unknown author kind: {other}"))),
    }
}

/// Row mirror of the `notes` table (SOUL §21). The core [`Note::author`] sum
/// type is stored split across `author_kind`/`author_id`; the store-only
/// `created_at` is exposed on the row but not modelled by the core [`Note`].
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct NoteRow {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub author_kind: String,
    pub author_id: Uuid,
    pub title: String,
    pub markdown: String,
    pub tags: Json<Vec<String>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl TryFrom<NoteRow> for Note {
    type Error = StoreError;

    fn try_from(r: NoteRow) -> Result<Self, Self::Error> {
        Ok(Note {
            id: NoteId::from_uuid(r.id),
            workspace_id: WorkspaceId::from_uuid(r.workspace_id),
            author: author_from_parts(&r.author_kind, r.author_id)?,
            title: r.title,
            markdown: r.markdown,
            tags: r.tags.0,
            updated_at: r.updated_at,
        })
    }
}

/// Row mirror of the `links` table. Both endpoints of the core [`Link`] are
/// stored split across `(from_kind, from_id)` / `(to_kind, to_id)` — the same
/// `SourceRef` encoding as `documents` (see [`source_from_parts`]). `author` is
/// split across `author_kind`/`author_id` like [`NoteRow`].
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct LinkRow {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub from_kind: String,
    pub from_id: String,
    pub to_kind: String,
    pub to_id: String,
    pub label: Option<String>,
    pub note: Option<String>,
    pub author_kind: String,
    pub author_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl TryFrom<LinkRow> for Link {
    type Error = StoreError;

    fn try_from(r: LinkRow) -> Result<Self, Self::Error> {
        Ok(Link {
            id: LinkId::from_uuid(r.id),
            workspace_id: WorkspaceId::from_uuid(r.workspace_id),
            from: source_from_parts(&r.from_kind, &r.from_id)?,
            to: source_from_parts(&r.to_kind, &r.to_id)?,
            label: r.label,
            note: r.note,
            author: author_from_parts(&r.author_kind, r.author_id)?,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
    }
}

/// Row mirror of the `object_labels` table (SOUL §9). A label on a stored file or
/// directory path, keyed by `(store, path)`; `author` is split across
/// `author_kind`/`author_id` like [`LinkRow`].
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ObjectLabelRow {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub store: String,
    pub path: String,
    pub is_dir: bool,
    pub label: String,
    pub author_kind: String,
    pub author_id: Uuid,
    pub created_at: DateTime<Utc>,
}

impl TryFrom<ObjectLabelRow> for ObjectLabel {
    type Error = StoreError;

    fn try_from(r: ObjectLabelRow) -> Result<Self, Self::Error> {
        Ok(ObjectLabel {
            id: ObjectLabelId::from_uuid(r.id),
            workspace_id: WorkspaceId::from_uuid(r.workspace_id),
            store: r.store,
            path: r.path,
            is_dir: r.is_dir,
            label: r.label,
            author: author_from_parts(&r.author_kind, r.author_id)?,
            created_at: r.created_at,
        })
    }
}

// ---------------------------------------------------------------------------
// per-App durable key/value store (SOUL §12/§29)
// ---------------------------------------------------------------------------

/// Row mirror of the `app_data` table — one `(app, key) → value` entry in a
/// workspace's per-App key/value store. The JSON document rides in the `value`
/// JSONB column; `app` is the namespace (a UI id on the handler path, or a
/// caller-named namespace otherwise, SOUL §12/§29). Workspace-scoped like every
/// other row.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AppDataRow {
    pub app: String,
    pub key: String,
    pub value: Json<serde_json::Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// emerged UIs (AI-authored declarative component trees)
// ---------------------------------------------------------------------------

/// Row mirror of the `ui_definitions` table. The core [`UiDefinition::author`]
/// sum type is stored split across `author_kind`/`author_id` (like notes); the
/// component tree rides in the `definition` JSONB column.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UiDefinitionRow {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub author_kind: String,
    pub author_id: Uuid,
    pub name: Option<String>,
    pub title: String,
    pub description: Option<String>,
    pub spec_version: i32,
    pub version: i64,
    pub definition: Json<UiSpec>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl TryFrom<UiDefinitionRow> for UiDefinition {
    type Error = StoreError;

    fn try_from(r: UiDefinitionRow) -> Result<Self, Self::Error> {
        Ok(UiDefinition {
            id: UiDefinitionId::from_uuid(r.id),
            workspace_id: WorkspaceId::from_uuid(r.workspace_id),
            author: author_from_parts(&r.author_kind, r.author_id)?,
            name: r.name,
            title: r.title,
            description: r.description,
            spec_version: r.spec_version.max(0) as u32,
            version: r.version,
            definition: r.definition.0,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
    }
}

// ---------------------------------------------------------------------------
// mcp endpoints (Boa-scripted scoped MCP endpoints, SOUL §26)
// ---------------------------------------------------------------------------

/// Row mirror of the `mcp_endpoints` table. The core [`McpEndpoint::author`] sum
/// type is stored split across `author_kind`/`author_id` (like ui_definitions);
/// the Boa program rides in the `script` TEXT column.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct McpEndpointRow {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub name: String,
    pub description: String,
    pub script: String,
    pub bucket_name: Option<String>,
    pub key_prefix: Option<String>,
    pub grant_id: Option<Uuid>,
    pub enabled: bool,
    pub author_kind: String,
    pub author_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl TryFrom<McpEndpointRow> for McpEndpoint {
    type Error = StoreError;

    fn try_from(r: McpEndpointRow) -> Result<Self, Self::Error> {
        Ok(McpEndpoint {
            id: McpEndpointId::from_uuid(r.id),
            workspace_id: WorkspaceId::from_uuid(r.workspace_id),
            name: r.name,
            description: r.description,
            script: r.script,
            bucket_name: r.bucket_name,
            key_prefix: r.key_prefix,
            grant_id: r.grant_id.map(GrantId::from_uuid),
            enabled: r.enabled,
            author: author_from_parts(&r.author_kind, r.author_id)?,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
    }
}

// ---------------------------------------------------------------------------
// pending questions (ask_user, SOUL §7/§12)
// ---------------------------------------------------------------------------

/// Row mirror of the `pending_questions` table — an unanswered `ask_user` form.
/// The questions ride in the `questions` JSONB column.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PendingQuestionRow {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub conversation_id: Uuid,
    pub questions: Json<Vec<Question>>,
    pub created_at: DateTime<Utc>,
    pub resolved_at: Option<DateTime<Utc>>,
    /// The user's structured answers, stamped when the form resolves with them;
    /// NULL while pending or when the question was superseded unanswered.
    pub answers: Option<Json<Vec<Answer>>>,
}

impl From<PendingQuestionRow> for PendingQuestion {
    fn from(r: PendingQuestionRow) -> Self {
        PendingQuestion {
            id: PendingQuestionId::from_uuid(r.id),
            workspace_id: WorkspaceId::from_uuid(r.workspace_id),
            conversation_id: ConversationId::from_uuid(r.conversation_id),
            questions: r.questions.0,
            created_at: r.created_at,
            resolved_at: r.resolved_at,
            answers: r.answers.map(|a| a.0),
        }
    }
}

/// Row mirror of the `pending_approvals` table — a deferred, guard-gated tool call
/// awaiting the user's Approve/Reject. `arguments` rides in JSONB; `decision` is
/// the lowercase ruling text (`approved`/`rejected`), NULL while pending.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PendingApprovalRow {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub conversation_id: Uuid,
    pub tool: String,
    pub arguments: Json<serde_json::Value>,
    pub reason: String,
    pub created_at: DateTime<Utc>,
    pub resolved_at: Option<DateTime<Utc>>,
    pub decision: Option<String>,
}

impl From<PendingApprovalRow> for PendingApproval {
    fn from(r: PendingApprovalRow) -> Self {
        PendingApproval {
            id: PendingApprovalId::from_uuid(r.id),
            workspace_id: WorkspaceId::from_uuid(r.workspace_id),
            conversation_id: ConversationId::from_uuid(r.conversation_id),
            tool: r.tool,
            arguments: r.arguments.0,
            reason: r.reason,
            created_at: r.created_at,
            resolved_at: r.resolved_at,
            decision: r.decision.as_deref().and_then(|d| match d {
                "approved" => Some(ApprovalDecision::Approved),
                "rejected" => Some(ApprovalDecision::Rejected),
                _ => None,
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// documents & chunks (ingest derivation, SOUL §5/§6.4/§10)
// ---------------------------------------------------------------------------

/// Split a core [`SourceRef`] into its stored `(source_kind, source_id)`
/// columns: a discriminator and the referenced id (a uuid string for
/// first-class rows, or a uri for `external`).
#[must_use]
pub fn source_to_parts(source: &SourceRef) -> (&'static str, String) {
    match source {
        SourceRef::Event { id } => ("event", id.to_string()),
        SourceRef::Object { id } => ("object", id.to_string()),
        SourceRef::Note { id } => ("note", id.to_string()),
        SourceRef::Memory { id } => ("memory", id.to_string()),
        SourceRef::Email { id } => ("email", id.to_string()),
        SourceRef::Message { id } => ("message", id.to_string()),
        SourceRef::Document { id } => ("document", id.to_string()),
        SourceRef::External { uri } => ("external", uri.clone()),
    }
}

/// Reassemble a core [`SourceRef`] from its stored `(source_kind, source_id)`
/// columns, or [`StoreError::Decode`] on an unknown discriminator / unparseable
/// id.
pub fn source_from_parts(kind: &str, id: &str) -> Result<SourceRef, StoreError> {
    let parse = |id: &str| id.parse::<Uuid>().map_err(StoreError::decode);
    Ok(match kind {
        "event" => SourceRef::Event {
            id: EventId::from_uuid(parse(id)?),
        },
        "object" => SourceRef::Object {
            id: ObjectId::from_uuid(parse(id)?),
        },
        "note" => SourceRef::Note {
            id: NoteId::from_uuid(parse(id)?),
        },
        "memory" => SourceRef::Memory {
            id: MemoryId::from_uuid(parse(id)?),
        },
        "email" => SourceRef::Email {
            id: EmailId::from_uuid(parse(id)?),
        },
        "message" => SourceRef::Message {
            id: MessageId::from_uuid(parse(id)?),
        },
        "document" => SourceRef::Document {
            id: DocumentId::from_uuid(parse(id)?),
        },
        "external" => SourceRef::External { uri: id.to_owned() },
        other => return Err(StoreError::decode(format!("unknown source kind: {other}"))),
    })
}

/// Row mirror of the `documents` table. The core [`Document::source`]
/// [`SourceRef`] is stored split across `source_kind`/`source_id`; store-only
/// timestamps are exposed on the row but not modelled by the core type.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DocumentRow {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub source_kind: String,
    pub source_id: String,
    pub text: String,
    pub summary: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl TryFrom<DocumentRow> for Document {
    type Error = StoreError;

    fn try_from(r: DocumentRow) -> Result<Self, Self::Error> {
        Ok(Document {
            id: DocumentId::from_uuid(r.id),
            workspace_id: WorkspaceId::from_uuid(r.workspace_id),
            source: source_from_parts(&r.source_kind, &r.source_id)?,
            text: r.text,
            summary: r.summary,
        })
    }
}

/// Row mirror of the `chunks` table (SOUL §6.4). `point_id` is the Qdrant point
/// handle, NULL until the chunk is embedded.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ChunkRow {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub document_id: Uuid,
    pub ordinal: i32,
    pub text: String,
    pub point_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

impl From<ChunkRow> for Chunk {
    fn from(r: ChunkRow) -> Self {
        Chunk {
            id: ChunkId::from_uuid(r.id),
            workspace_id: WorkspaceId::from_uuid(r.workspace_id),
            document_id: DocumentId::from_uuid(r.document_id),
            ordinal: r.ordinal,
            text: r.text,
            qdrant_point_id: r.point_id,
        }
    }
}

// ---------------------------------------------------------------------------
// memories (personalization, SOUL §22)
// ---------------------------------------------------------------------------

/// The stored `scope` token for a [`MemoryScope`] ('user' | 'workspace').
#[must_use]
pub fn memory_scope_to_text(scope: MemoryScope) -> &'static str {
    match scope {
        MemoryScope::User => "user",
        MemoryScope::Workspace => "workspace",
    }
}

/// Row mirror of the `memories` table (SOUL §22). `scope` ('user'|'workspace')
/// maps to [`MemoryScope`]; `user_id` is set for a 'user' memory. The optional
/// [`SourceRef`] is stored split across `source_kind`/`source_id` (both NULL when
/// absent). The store-only `updated_at` is exposed but not modelled by the core
/// [`Memory`].
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MemoryRow {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub scope: String,
    pub user_id: Option<Uuid>,
    pub text: String,
    pub source_kind: Option<String>,
    pub source_id: Option<String>,
    pub point_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl TryFrom<MemoryRow> for Memory {
    type Error = StoreError;

    fn try_from(r: MemoryRow) -> Result<Self, Self::Error> {
        let source = match (r.source_kind.as_deref(), r.source_id.as_deref()) {
            (Some(kind), Some(id)) => Some(source_from_parts(kind, id)?),
            _ => None,
        };
        Ok(Memory {
            id: MemoryId::from_uuid(r.id),
            workspace_id: WorkspaceId::from_uuid(r.workspace_id),
            scope: enum_from_text::<MemoryScope>(&r.scope)?,
            user_id: r.user_id.map(UserId::from_uuid),
            text: r.text,
            source,
            point_id: r.point_id,
            created_at: r.created_at,
        })
    }
}

/// Row mirror of the `profiles` table (SOUL §22). `fields` is the flat JSON
/// object of per-user details; store-only timestamps are not modelled by the
/// core [`Profile`].
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ProfileRow {
    pub workspace_id: Uuid,
    pub user_id: Uuid,
    pub fields: Json<Map>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<ProfileRow> for Profile {
    fn from(r: ProfileRow) -> Self {
        Profile {
            workspace_id: WorkspaceId::from_uuid(r.workspace_id),
            user_id: UserId::from_uuid(r.user_id),
            fields: r.fields.0,
        }
    }
}

/// Row mirror of the `llm_settings` table (SOUL §7/§13). Each model/voice column
/// is nullable — `NULL` means "unset, fall back to the `[llm]` config default";
/// microphone speed is concrete and defaults to 1.5×. Store-only timestamps are
/// not modelled by the core [`LlmSettings`].
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct LlmSettingsRow {
    pub workspace_id: Uuid,
    pub user_id: Uuid,
    pub chat_model: Option<String>,
    pub speech_model: Option<String>,
    pub speech_voice: Option<String>,
    pub transcription_model: Option<String>,
    pub voice_input_speed: f32,
    pub ocr_model: Option<String>,
    pub image_input_models: Json<Vec<String>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<LlmSettingsRow> for LlmSettings {
    fn from(r: LlmSettingsRow) -> Self {
        LlmSettings {
            workspace_id: WorkspaceId::from_uuid(r.workspace_id),
            user_id: UserId::from_uuid(r.user_id),
            chat_model: r.chat_model,
            speech_model: r.speech_model,
            speech_voice: r.speech_voice,
            transcription_model: r.transcription_model,
            voice_input_speed: r.voice_input_speed,
            ocr_model: r.ocr_model,
            image_input_models: r.image_input_models.0,
        }
    }
}

/// Row mirror of the `search_settings` table (SOUL §7/§13). `default_provider` is
/// nullable — `NULL` means "unset, fall back to the `[search].backend` config
/// default"; store-only timestamps are not modelled by the core [`SearchSettings`].
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SearchSettingsRow {
    pub workspace_id: Uuid,
    pub user_id: Uuid,
    pub default_provider: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<SearchSettingsRow> for SearchSettings {
    fn from(r: SearchSettingsRow) -> Self {
        SearchSettings {
            workspace_id: WorkspaceId::from_uuid(r.workspace_id),
            user_id: UserId::from_uuid(r.user_id),
            default_provider: r.default_provider,
        }
    }
}

/// Row mirror of the `storage_settings` table (SOUL §7/§9/§13). `default_store`
/// is nullable — `NULL` means "unset, fall back to the `[storage]` config
/// default"; store-only timestamps are not modelled by the core
/// [`StorageSettings`].
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct StorageSettingsRow {
    pub workspace_id: Uuid,
    pub user_id: Uuid,
    pub default_store: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<StorageSettingsRow> for StorageSettings {
    fn from(r: StorageSettingsRow) -> Self {
        StorageSettings {
            workspace_id: WorkspaceId::from_uuid(r.workspace_id),
            user_id: UserId::from_uuid(r.user_id),
            default_store: r.default_store,
        }
    }
}

// ---------------------------------------------------------------------------
// skills (SOUL §23)
// ---------------------------------------------------------------------------

/// Row mirror of the `skills` table (SOUL §23). `tools` is a JSONB array of tool
/// names; `code` is an optional JSONB [`Code`] (NULL for a pure-instructions
/// skill). Store-only timestamps are not modelled by the core [`Skill`].
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SkillRow {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub name: String,
    pub description: String,
    pub instructions_md: String,
    pub tools: Json<Vec<String>>,
    pub code: Option<Json<Code>>,
    pub advertised: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<SkillRow> for Skill {
    fn from(r: SkillRow) -> Self {
        Skill {
            id: SkillId::from_uuid(r.id),
            workspace_id: WorkspaceId::from_uuid(r.workspace_id),
            name: r.name,
            description: r.description,
            instructions_md: r.instructions_md,
            tools: r.tools.0,
            code: r.code.map(|j| j.0),
            advertised: r.advertised,
        }
    }
}

// ---------------------------------------------------------------------------
// agent profiles (SOUL §19/§25)
// ---------------------------------------------------------------------------

/// Row mirror of the `agent_profiles` table (SOUL §19). The `tools`/`skills`/
/// `subagents`/`channels` JSONB arrays are name lists; `model`/
/// `system_prompt`/`grant_id` are nullable. Store-only timestamps are not modelled
/// by the core [`AgentProfile`].
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AgentProfileRow {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub name: String,
    pub model: Option<String>,
    pub system_prompt: Option<String>,
    pub tools: Json<Vec<String>>,
    pub skills: Json<Vec<String>>,
    pub subagents: Json<Vec<String>>,
    pub channels: Json<Vec<String>>,
    pub grant_id: Option<Uuid>,
    /// Optional per-profile tool guard (SOUL §19), NULL for an unguarded profile.
    pub guard: Option<Json<ToolGuard>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<AgentProfileRow> for AgentProfile {
    fn from(r: AgentProfileRow) -> Self {
        AgentProfile {
            id: AgentProfileId::from_uuid(r.id),
            workspace_id: WorkspaceId::from_uuid(r.workspace_id),
            name: r.name,
            model: r.model,
            system_prompt: r.system_prompt,
            tools: r.tools.0,
            skills: r.skills.0,
            subagents: r.subagents.0,
            channels: r.channels.0,
            grant_id: r.grant_id.map(GrantId::from_uuid),
            guard: r.guard.map(|j| j.0),
        }
    }
}

/// Row mirror of the `mcp_servers` table (SOUL §26). `args`/`tools` are JSONB
/// name arrays; `env` a JSONB string map; `auth` the JSONB [`McpAuthSpec`].
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct McpServerRow {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub name: String,
    pub transport: String,
    pub command: String,
    pub args: Json<Vec<String>>,
    pub env: Json<std::collections::BTreeMap<String, String>>,
    pub url: String,
    pub auth: Json<McpAuthSpec>,
    pub enabled: bool,
    pub tools: Json<Vec<String>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<McpServerRow> for McpServerDef {
    fn from(r: McpServerRow) -> Self {
        McpServerDef {
            id: McpServerId::from_uuid(r.id),
            workspace_id: WorkspaceId::from_uuid(r.workspace_id),
            name: r.name,
            transport: r.transport,
            command: r.command,
            args: r.args.0,
            env: r.env.0,
            url: r.url,
            auth: r.auth.0,
            enabled: r.enabled,
            tools: r.tools.0,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

/// The stored `status` token for a [`TerminalSessionStatus`].
#[must_use]
pub fn terminal_session_status_to_text(status: TerminalSessionStatus) -> &'static str {
    match status {
        TerminalSessionStatus::Active => "active",
        TerminalSessionStatus::Closed => "closed",
        TerminalSessionStatus::Failed => "failed",
    }
}

/// Row mirror of the `terminal_sessions` table (SOUL §20). `backend`/`status`
/// are snake_case TEXT tokens parsed back into their enums.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TerminalSessionRow {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub backend: String,
    pub status: String,
    pub host_dir: Option<String>,
    pub sync_prefix: Option<String>,
    /// Owning pod (multi-pod HA, SOUL §16 M7); NULL for a pre-upgrade row.
    pub pod_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
}

impl TryFrom<TerminalSessionRow> for TerminalSession {
    type Error = StoreError;
    fn try_from(r: TerminalSessionRow) -> Result<Self, StoreError> {
        Ok(TerminalSession {
            id: TerminalSessionId::from_uuid(r.id),
            workspace_id: WorkspaceId::from_uuid(r.workspace_id),
            backend: ExecutorKind::parse_token(&r.backend).ok_or_else(|| {
                StoreError::decode(format!("unknown executor backend `{}`", r.backend))
            })?,
            status: enum_from_text::<TerminalSessionStatus>(&r.status)?,
            host_dir: r.host_dir,
            sync_prefix: r.sync_prefix,
            pod_id: r.pod_id,
            created_at: r.created_at,
            closed_at: r.closed_at,
        })
    }
}

/// The stored `status` token for a [`SandboxState`].
#[must_use]
pub fn sandbox_state_to_text(state: SandboxState) -> &'static str {
    match state {
        SandboxState::Pending => "pending",
        SandboxState::Ready => "ready",
        SandboxState::Failed => "failed",
        SandboxState::Stopped => "stopped",
    }
}

/// Row mirror of the `workspace_sandboxes` table (SOUL §20). `backend`/`status`
/// are snake_case TEXT tokens parsed back into their enums.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct WorkspaceSandboxRow {
    pub workspace_id: Uuid,
    pub backend: String,
    pub image: String,
    pub status: String,
    pub container_ref: Option<String>,
    pub volume_ref: Option<String>,
    /// Owning pod (multi-pod HA, SOUL §16 M7); NULL for a pre-upgrade row.
    pub pod_id: Option<String>,
    pub last_activity: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl TryFrom<WorkspaceSandboxRow> for WorkspaceSandboxRecord {
    type Error = StoreError;
    fn try_from(r: WorkspaceSandboxRow) -> Result<Self, StoreError> {
        Ok(WorkspaceSandboxRecord {
            workspace_id: WorkspaceId::from_uuid(r.workspace_id),
            backend: ExecutorKind::parse_token(&r.backend).ok_or_else(|| {
                StoreError::decode(format!("unknown executor backend `{}`", r.backend))
            })?,
            image: r.image,
            status: enum_from_text::<SandboxState>(&r.status)?,
            container_ref: r.container_ref,
            volume_ref: r.volume_ref,
            pod_id: r.pod_id,
            last_activity: r.last_activity,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
    }
}

/// Row mirror of the `automations` table (SOUL §11). `triggers`/`actions` are
/// JSONB arrays of arbitrary typed specs; `condition`/`spec` are optional JSONB
/// blobs. The engine in `catalerum-automation` owns their typed interpretation;
/// here they round-trip as [`serde_json::Value`]. Store-only timestamps are not
/// modelled by the core [`Automation`].
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AutomationRow {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub name: String,
    pub enabled: bool,
    pub triggers: Json<Vec<serde_json::Value>>,
    pub condition: Option<Json<serde_json::Value>>,
    pub actions: Json<Vec<serde_json::Value>>,
    pub spec: Option<Json<serde_json::Value>>,
    pub grant_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<AutomationRow> for Automation {
    fn from(r: AutomationRow) -> Self {
        Automation {
            id: AutomationId::from_uuid(r.id),
            workspace_id: WorkspaceId::from_uuid(r.workspace_id),
            name: r.name,
            enabled: r.enabled,
            triggers: r.triggers.0,
            condition: r.condition.map(|j| j.0),
            actions: r.actions.0,
            spec: r.spec.map(|j| j.0),
            grant_id: r.grant_id.map(GrantId::from_uuid),
        }
    }
}

/// Row mirror of the `automation_runs` table (SOUL §11). `status` is lowercase
/// TEXT parsed back into [`RunStatus`]; `trigger` is the optional JSONB fired-by
/// payload.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AutomationRunRow {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub automation_id: Uuid,
    pub status: String,
    pub grant_id: Option<Uuid>,
    pub trigger: Option<Json<serde_json::Value>>,
    pub error: Option<String>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

impl TryFrom<AutomationRunRow> for AutomationRun {
    type Error = StoreError;
    fn try_from(r: AutomationRunRow) -> Result<Self, StoreError> {
        Ok(AutomationRun {
            id: AutomationRunId::from_uuid(r.id),
            workspace_id: WorkspaceId::from_uuid(r.workspace_id),
            automation_id: AutomationId::from_uuid(r.automation_id),
            status: enum_from_text::<RunStatus>(&r.status)?,
            grant_id: r.grant_id.map(GrantId::from_uuid),
            trigger: r.trigger.map(|j| j.0),
            error: r.error,
            started_at: r.started_at,
            finished_at: r.finished_at,
        })
    }
}

/// Row mirror of the `automation_steps` table (SOUL §11). `status` is lowercase
/// TEXT parsed back into [`StepStatus`]; `action`/`output` are JSONB.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AutomationStepRow {
    pub id: Uuid,
    pub run_id: Uuid,
    pub workspace_id: Uuid,
    pub ordinal: i32,
    pub action: Json<serde_json::Value>,
    pub status: String,
    pub output: Option<Json<serde_json::Value>>,
    pub error: Option<String>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

impl TryFrom<AutomationStepRow> for AutomationStep {
    type Error = StoreError;
    fn try_from(r: AutomationStepRow) -> Result<Self, StoreError> {
        Ok(AutomationStep {
            id: AutomationStepId::from_uuid(r.id),
            run_id: AutomationRunId::from_uuid(r.run_id),
            workspace_id: WorkspaceId::from_uuid(r.workspace_id),
            ordinal: r.ordinal,
            action: r.action.0,
            status: enum_from_text::<StepStatus>(&r.status)?,
            output: r.output.map(|j| j.0),
            error: r.error,
            started_at: r.started_at,
            finished_at: r.finished_at,
        })
    }
}

/// The stored `status` token for a [`RunStatus`] (matches the snake_case serde form).
#[must_use]
pub fn run_status_to_text(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Running => "running",
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed => "failed",
        RunStatus::Cancelled => "cancelled",
    }
}

/// The stored `status` token for a [`StepStatus`] (matches the snake_case serde form).
#[must_use]
pub fn step_status_to_text(status: StepStatus) -> &'static str {
    match status {
        StepStatus::Running => "running",
        StepStatus::Succeeded => "succeeded",
        StepStatus::Failed => "failed",
        StepStatus::Skipped => "skipped",
    }
}

// ---------------------------------------------------------------------------
// tasks & Kanban board (SOUL §24)
// ---------------------------------------------------------------------------

/// The stored `status` token for a [`TaskStatus`].
#[must_use]
pub fn task_status_to_text(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Open => "open",
        TaskStatus::InProgress => "in_progress",
        TaskStatus::Blocked => "blocked",
        TaskStatus::Done => "done",
    }
}

/// Row mirror of the `boards` table. The core [`Board::columns`] are loaded
/// separately (from `board_columns`) and assembled by the repository.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct BoardRow {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Row mirror of the `board_columns` table.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ColumnRow {
    pub id: Uuid,
    pub board_id: Uuid,
    pub name: String,
    pub ordinal: i32,
}

impl From<ColumnRow> for Column {
    fn from(r: ColumnRow) -> Self {
        Column {
            id: ColumnId::from_uuid(r.id),
            name: r.name,
            order: r.ordinal,
        }
    }
}

/// Assemble a core [`Board`] from its row and (ordered) columns.
#[must_use]
pub fn board_from_parts(row: BoardRow, columns: Vec<ColumnRow>) -> Board {
    Board {
        id: BoardId::from_uuid(row.id),
        workspace_id: WorkspaceId::from_uuid(row.workspace_id),
        name: row.name,
        columns: columns.into_iter().map(Column::from).collect(),
    }
}

/// Row mirror of the `tasks` table (SOUL §24). The optional core
/// [`Task::assignee`] is stored split across `assignee_kind`/`assignee_id`
/// (both NULL when unassigned); `status` maps to [`TaskStatus`].
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TaskRow {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub board_id: Uuid,
    pub column_id: Uuid,
    pub title: String,
    pub body_md: String,
    pub assignee_kind: Option<String>,
    pub assignee_id: Option<Uuid>,
    pub ordinal: i32,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl TryFrom<TaskRow> for Task {
    type Error = StoreError;

    fn try_from(r: TaskRow) -> Result<Self, Self::Error> {
        let assignee = match (r.assignee_kind.as_deref(), r.assignee_id) {
            (Some(kind), Some(id)) => Some(author_from_parts(kind, id)?),
            _ => None,
        };
        Ok(Task {
            id: TaskId::from_uuid(r.id),
            workspace_id: WorkspaceId::from_uuid(r.workspace_id),
            board_id: BoardId::from_uuid(r.board_id),
            column_id: ColumnId::from_uuid(r.column_id),
            title: r.title,
            body_md: r.body_md,
            assignee,
            order: r.ordinal,
            status: enum_from_text::<TaskStatus>(&r.status)?,
        })
    }
}

// ---------------------------------------------------------------------------
// connections
// ---------------------------------------------------------------------------

/// Row mirror of the `connections` table. Carries the store-only `config`
/// (per-provider settings), `name`, and timestamps that the core
/// [`Connection`] does not model; `sync_token` maps to the core `cursor`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ConnectionRow {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub kind: String,
    pub name: String,
    pub credential_ref: Option<String>,
    pub config: Json<serde_json::Value>,
    pub sync_token: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl ConnectionRow {
    /// The per-provider settings blob (local dir path, CalDAV base URL, …).
    #[must_use]
    pub fn config(&self) -> &serde_json::Value {
        &self.config.0
    }
}

impl TryFrom<ConnectionRow> for Connection {
    type Error = StoreError;

    fn try_from(r: ConnectionRow) -> Result<Self, Self::Error> {
        Ok(Connection {
            id: ConnectionId::from_uuid(r.id),
            workspace_id: WorkspaceId::from_uuid(r.workspace_id),
            kind: enum_from_text::<ConnectionKind>(&r.kind)?,
            name: r.name,
            credential_ref: r.credential_ref,
            cursor: r.sync_token.map(Cursor::new),
        })
    }
}

/// Encode a [`ConnectionKind`] to its stored `TEXT` token.
pub fn connection_kind_to_text(kind: ConnectionKind) -> Result<String, StoreError> {
    enum_to_text(&kind)
}

// ---------------------------------------------------------------------------
// calendars
// ---------------------------------------------------------------------------

/// Row mirror of the `calendars` table.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CalendarRow {
    pub id: Uuid,
    pub workspace_id: Uuid,
    /// `NULL` for a local (database-native) calendar with no provider connection.
    pub connection_id: Option<Uuid>,
    pub external_id: String,
    pub name: String,
    pub read_only: bool,
    pub created_at: DateTime<Utc>,
}

impl From<CalendarRow> for Calendar {
    fn from(r: CalendarRow) -> Self {
        Calendar {
            id: CalendarId::from_uuid(r.id),
            workspace_id: WorkspaceId::from_uuid(r.workspace_id),
            connection_id: r.connection_id.map(ConnectionId::from_uuid),
            external_id: r.external_id,
            name: r.name,
            read_only: r.read_only,
        }
    }
}

// ---------------------------------------------------------------------------
// buckets, objects (the storage catalogue, SOUL §9)
// ---------------------------------------------------------------------------

/// Row mirror of the `buckets` table.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct BucketRow {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub connection_id: Uuid,
    pub name: String,
    pub prefix: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl From<BucketRow> for Bucket {
    fn from(r: BucketRow) -> Self {
        Bucket {
            id: BucketId::from_uuid(r.id),
            workspace_id: WorkspaceId::from_uuid(r.workspace_id),
            connection_id: ConnectionId::from_uuid(r.connection_id),
            name: r.name,
            prefix: r.prefix,
        }
    }
}

/// Row mirror of the `objects` table. The catalogued metadata for one stored
/// object; the blob itself stays in the bucket (§14). `size` is BIGINT (i64) in
/// Postgres and maps onto the core `u64` (negatives, impossible here, clamp to 0).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ObjectRow {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub bucket_id: Uuid,
    pub key: String,
    pub size: i64,
    pub content_type: Option<String>,
    pub etag: Option<String>,
    pub last_modified: DateTime<Utc>,
    pub sha256: Option<String>,
    pub extracted_text_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<ObjectRow> for StoredObject {
    fn from(r: ObjectRow) -> Self {
        StoredObject {
            id: ObjectId::from_uuid(r.id),
            workspace_id: WorkspaceId::from_uuid(r.workspace_id),
            bucket_id: BucketId::from_uuid(r.bucket_id),
            key: r.key,
            size: r.size.max(0) as u64,
            content_type: r.content_type,
            etag: r.etag,
            last_modified: r.last_modified,
            sha256: r.sha256,
            extracted_text_id: r.extracted_text_id.map(DocumentId::from_uuid),
        }
    }
}

// ---------------------------------------------------------------------------
// events
// ---------------------------------------------------------------------------

/// Row mirror of the `events` table. The store-only `created_at` and
/// `updated_at` columns are exposed on the row; the core [`Event`] maps
/// `starts_at`/`ends_at` onto its `start`/`end` fields and carries `all_day`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct EventRow {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub calendar_id: Uuid,
    pub uid: String,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    pub all_day: bool,
    pub rrule: Option<String>,
    pub summary: String,
    pub location: Option<String>,
    pub body: Option<String>,
    pub attendees: Json<Vec<EntityRef>>,
    pub labels: Json<Vec<String>>,
    pub attachments: Json<Vec<Attachment>>,
    pub etag: Option<String>,
    pub sequence: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<EventRow> for Event {
    fn from(r: EventRow) -> Self {
        Event {
            id: EventId::from_uuid(r.id),
            workspace_id: WorkspaceId::from_uuid(r.workspace_id),
            calendar_id: CalendarId::from_uuid(r.calendar_id),
            uid: r.uid,
            start: r.starts_at,
            end: r.ends_at,
            all_day: r.all_day,
            rrule: r.rrule,
            summary: r.summary,
            location: r.location,
            attendees: r.attendees.0,
            body: r.body,
            labels: r.labels.0,
            attachments: r.attachments.0,
            etag: r.etag,
            sequence: i64::from(r.sequence),
        }
    }
}

// ---------------------------------------------------------------------------
// mailboxes, emails (read-only email ingest, SOUL §28)
// ---------------------------------------------------------------------------

/// Row mirror of the `mailboxes` table.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MailboxRow {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub connection_id: Uuid,
    pub external_id: String,
    pub name: String,
    pub read_only: bool,
    pub created_at: DateTime<Utc>,
}

impl From<MailboxRow> for Mailbox {
    fn from(r: MailboxRow) -> Self {
        Mailbox {
            id: MailboxId::from_uuid(r.id),
            workspace_id: WorkspaceId::from_uuid(r.workspace_id),
            connection_id: ConnectionId::from_uuid(r.connection_id),
            external_id: r.external_id,
            name: r.name,
            read_only: r.read_only,
        }
    }
}

/// Row mirror of the `grants` table (SOUL §19). `capabilities`/`constraints` ride
/// in JSONB.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct GrantRow {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub name: String,
    pub capabilities: Json<Vec<Capability>>,
    pub constraints: Json<Constraints>,
    pub created_at: DateTime<Utc>,
}

impl From<GrantRow> for Grant {
    fn from(r: GrantRow) -> Self {
        Grant {
            id: GrantId::from_uuid(r.id),
            workspace_id: WorkspaceId::from_uuid(r.workspace_id),
            name: r.name,
            capabilities: r.capabilities.0,
            constraints: r.constraints.0,
        }
    }
}

/// Row mirror of the `emails` table. `from_addr`/`to_addrs`/`cc_addrs`/`flags`
/// ride in JSONB; `from_addr` is a JSON `null` when the message has no parseable
/// `From`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct EmailRow {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub mailbox_id: Uuid,
    pub uid: String,
    pub message_id: Option<String>,
    pub from_addr: Json<Option<EmailAddress>>,
    pub to_addrs: Json<Vec<EmailAddress>>,
    pub cc_addrs: Json<Vec<EmailAddress>>,
    pub subject: String,
    pub received_at: Option<DateTime<Utc>>,
    pub body_text: Option<String>,
    pub body_html: Option<String>,
    pub has_attachments: bool,
    pub flags: Json<Vec<String>>,
    pub labels: Json<Vec<String>>,
    pub raw_ref: Option<String>,
    pub attachments: Json<Vec<Attachment>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<EmailRow> for Email {
    fn from(r: EmailRow) -> Self {
        Email {
            id: EmailId::from_uuid(r.id),
            workspace_id: WorkspaceId::from_uuid(r.workspace_id),
            mailbox_id: MailboxId::from_uuid(r.mailbox_id),
            uid: r.uid,
            message_id: r.message_id,
            from: r.from_addr.0,
            to: r.to_addrs.0,
            cc: r.cc_addrs.0,
            subject: r.subject,
            received_at: r.received_at,
            body_text: r.body_text,
            body_html: r.body_html,
            has_attachments: r.has_attachments,
            flags: r.flags.0,
            labels: r.labels.0,
            raw_ref: r.raw_ref,
            attachments: r.attachments.0,
            raw: None,
        }
    }
}

// ---------------------------------------------------------------------------
// job_queue
// ---------------------------------------------------------------------------

/// The lifecycle status of a [`JobRow`] in the durable `job_queue` (SOUL §6.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    /// Awaiting a worker; eligible once `run_after <= now()`.
    Pending,
    /// Leased by a worker (`locked_by`) and in progress.
    Running,
    /// Completed successfully (terminal).
    Done,
    /// Exhausted its retries and gave up (terminal).
    Failed,
}

impl JobStatus {
    /// The stored `TEXT` token for this status.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            JobStatus::Pending => "pending",
            JobStatus::Running => "running",
            JobStatus::Done => "done",
            JobStatus::Failed => "failed",
        }
    }

    /// Parse a status from its stored `TEXT` token.
    pub fn parse(text: &str) -> Result<Self, StoreError> {
        match text {
            "pending" => Ok(JobStatus::Pending),
            "running" => Ok(JobStatus::Running),
            "done" => Ok(JobStatus::Done),
            "failed" => Ok(JobStatus::Failed),
            other => Err(StoreError::decode(format!("unknown job status: {other}"))),
        }
    }
}

/// A durable work-queue job (SOUL §6.2). Store-only — there is no
/// `catalerum-core` analogue. `workspace_id` is `None` for global maintenance
/// jobs; `payload` is the job's typed arguments as JSON.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct JobRow {
    pub id: Uuid,
    pub workspace_id: Option<Uuid>,
    pub kind: String,
    pub payload: Json<serde_json::Value>,
    pub status: String,
    pub attempts: i32,
    pub run_after: DateTime<Utc>,
    pub locked_at: Option<DateTime<Utc>>,
    pub locked_by: Option<String>,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl JobRow {
    /// The owning workspace as a typed id, if this is a tenant job.
    #[must_use]
    pub fn workspace_id(&self) -> Option<WorkspaceId> {
        self.workspace_id.map(WorkspaceId::from_uuid)
    }

    /// The job's typed arguments as JSON.
    #[must_use]
    pub fn payload(&self) -> &serde_json::Value {
        &self.payload.0
    }

    /// The parsed lifecycle status.
    pub fn status(&self) -> Result<JobStatus, StoreError> {
        JobStatus::parse(&self.status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_round_trips_through_text() {
        for role in [Role::Owner, Role::Admin, Role::Member, Role::Viewer] {
            let text = role_to_text(role).unwrap();
            let back: Role = enum_from_text(&text).unwrap();
            assert_eq!(role, back);
        }
    }

    #[test]
    fn message_role_round_trips() {
        for role in [
            MessageRole::System,
            MessageRole::User,
            MessageRole::Assistant,
            MessageRole::Tool,
        ] {
            let text = message_role_to_text(role).unwrap();
            let back: MessageRole = enum_from_text(&text).unwrap();
            assert_eq!(role, back);
        }
    }

    #[test]
    fn origin_round_trips() {
        for origin in [
            Origin::Web,
            Origin::Automation,
            Origin::Channel,
            Origin::Mcp,
        ] {
            let text = origin_to_text(origin).unwrap();
            let back: Origin = enum_from_text(&text).unwrap();
            assert_eq!(origin, back);
        }
    }

    #[test]
    fn role_text_is_snake_case() {
        assert_eq!(role_to_text(Role::Owner).unwrap(), "owner");
        assert_eq!(
            message_role_to_text(MessageRole::Assistant).unwrap(),
            "assistant"
        );
        assert_eq!(origin_to_text(Origin::Mcp).unwrap(), "mcp");
        assert_eq!(origin_to_text(Origin::Automation).unwrap(), "automation");
    }

    #[test]
    fn connection_kind_round_trips() {
        for kind in [
            ConnectionKind::Calendar,
            ConnectionKind::Storage,
            ConnectionKind::Channel,
        ] {
            let text = connection_kind_to_text(kind).unwrap();
            let back: ConnectionKind = enum_from_text(&text).unwrap();
            assert_eq!(kind, back);
        }
        assert_eq!(
            connection_kind_to_text(ConnectionKind::Calendar).unwrap(),
            "calendar"
        );
    }

    #[test]
    fn run_status_round_trips_through_text() {
        for status in [
            RunStatus::Running,
            RunStatus::Succeeded,
            RunStatus::Failed,
            RunStatus::Cancelled,
        ] {
            let back: RunStatus = enum_from_text(run_status_to_text(status)).unwrap();
            assert_eq!(status, back, "{status:?} round-trips");
        }
        assert_eq!(run_status_to_text(RunStatus::Succeeded), "succeeded");
    }

    #[test]
    fn step_status_round_trips_through_text() {
        for status in [
            StepStatus::Running,
            StepStatus::Succeeded,
            StepStatus::Failed,
            StepStatus::Skipped,
        ] {
            let back: StepStatus = enum_from_text(step_status_to_text(status)).unwrap();
            assert_eq!(status, back, "{status:?} round-trips");
        }
        assert_eq!(step_status_to_text(StepStatus::Skipped), "skipped");
    }

    #[test]
    fn author_round_trips_through_parts() {
        let user = Author::User { id: UserId::new() };
        let (kind, id) = author_to_parts(user);
        assert_eq!(kind, "user");
        assert_eq!(author_from_parts(kind, id).unwrap(), user);

        let agent = Author::Agent { id: AgentId::new() };
        let (kind, id) = author_to_parts(agent);
        assert_eq!(kind, "agent");
        assert_eq!(author_from_parts(kind, id).unwrap(), agent);

        assert!(author_from_parts("bogus", Uuid::new_v4()).is_err());
    }

    #[test]
    fn job_status_round_trips_through_text() {
        for status in [
            JobStatus::Pending,
            JobStatus::Running,
            JobStatus::Done,
            JobStatus::Failed,
        ] {
            let text = status.as_str();
            let back = JobStatus::parse(text).unwrap();
            assert_eq!(status, back);
        }
        assert!(JobStatus::parse("bogus").is_err());
    }
}
