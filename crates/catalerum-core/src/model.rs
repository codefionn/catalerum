//! The canonical, provider-agnostic domain model (SOUL §5).
//!
//! Every tenant object carries a [`WorkspaceId`] — the tenancy boundary
//! (SOUL §18); cross-workspace access is impossible by construction. Types name
//! no concrete provider (SOUL §3.2); concrete calendars/buckets/channels are
//! reached only through the traits in [`crate::provider`].
//!
//! [`EntityRef`] and [`SourceRef`] are the stable typed pointers that let
//! Postgres rows, Neo4j nodes, and Qdrant points share identity (SOUL §5).

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::id::*;
use crate::model_ui::UiSpec;

/// A free-form JSON map used for the "loose" fields in §5 (profile fields,
/// automation specs, channel/connection config, constraint blobs).
pub type Map = BTreeMap<String, Json>;

// ---------------------------------------------------------------------------
// Shared typed pointers
// ---------------------------------------------------------------------------

/// A stable, typed pointer to a domain entity, shared across Postgres / Neo4j /
/// Qdrant so the three stores agree on identity (SOUL §5). `EntityRef` points at
/// a catalogued [`Entity`] (person/org/topic/…); see [`SourceRef`] for the
/// concrete row a derived artifact came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntityRef {
    /// Workspace the entity belongs to.
    pub workspace_id: WorkspaceId,
    /// The catalogued entity.
    pub entity_id: EntityId,
    /// What kind of entity this points at (denormalized for fast reads).
    pub kind: EntityKind,
    /// Optional human label (denormalized display name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

/// A stable, typed pointer to the *source* a derived artifact (document, chunk,
/// memory, graph node) was produced from. Lets the graph/vector layers trace a
/// point back to the Postgres truth (SOUL §3.1, §5).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceRef {
    /// A calendar event.
    Event { id: EventId },
    /// A stored object (file/blob metadata row).
    Object { id: ObjectId },
    /// A markdown note.
    Note { id: NoteId },
    /// A curated memory.
    Memory { id: MemoryId },
    /// An ingested email message (SOUL §28).
    Email { id: EmailId },
    /// A message in a conversation.
    Message { id: MessageId },
    /// A document (extracted text container).
    Document { id: DocumentId },
    /// An external resource not yet modelled as a first-class row.
    External { uri: String },
}

// ---------------------------------------------------------------------------
// Workspace, identity, roles
// ---------------------------------------------------------------------------

/// An **organisation** — the administrative grouping above the tenancy boundary
/// (SOUL §18). Every [`Workspace`] belongs to exactly one organisation. Org
/// membership + roles govern administration only (creating/archiving workspaces,
/// org members, org policy) and confer **no** data access: the workspace stays
/// the sole data + capability boundary, and organisations never appear in
/// capability strings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Organisation {
    pub id: OrganisationId,
    pub name: String,
    /// URL-safe unique handle.
    pub slug: String,
    /// Org policy: who may create workspaces in this organisation. Deny-by-default
    /// (SOUL §18); the default is set at creation time from the deployment mode
    /// (`members` in single-user, `admins` in multi-user).
    #[serde(default)]
    pub workspace_creation: CreationPolicy,
}

/// An organisation role (SOUL §18). Administrative only — it never widens data
/// or capability access, which remain workspace-scoped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrgRole {
    Owner,
    Admin,
    Member,
}

/// Binds a [`User`] to an [`Organisation`] with an [`OrgRole`] (SOUL §18).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrgMembership {
    pub organisation_id: OrganisationId,
    pub user_id: UserId,
    pub role: OrgRole,
}

/// A deny-by-default creation policy (SOUL §18). Governs who may create
/// organisations (instance policy `organisation_creation`) or workspaces within an
/// organisation (org policy `workspace_creation`). The values carry the same
/// meaning in both contexts: `Disabled` forbids it, `Admins` restricts it to
/// org owners/admins, `Members` opens it to any org member.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CreationPolicy {
    /// Nobody may create (via the policy-gated API path).
    #[default]
    Disabled,
    /// Only org owners/admins may create.
    Admins,
    /// Any org member may create.
    Members,
}

/// The tenancy boundary. Every object belongs to exactly one workspace, and every
/// workspace belongs to exactly one [`Organisation`] (SOUL §18).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: WorkspaceId,
    /// The administrative organisation this workspace belongs to (SOUL §18).
    pub organisation_id: OrganisationId,
    pub name: String,
    /// URL-safe unique handle.
    pub slug: String,
    /// When the workspace was **soft-archived** by an org admin, or `None` while
    /// active (SOUL §18). An archived workspace is hidden from every default
    /// listing and cannot be switched into, but its data is retained so an org
    /// admin can restore it; archive replaced the former hard delete. Serde is
    /// additive — an absent field decodes as `None` (active).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<DateTime<Utc>>,
}

/// An SSO subject identifier (`iss`/`sub` pair, or an opaque provider subject).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subject {
    /// Issuer (OIDC `iss` / SAML entity id).
    pub issuer: String,
    /// Subject (OIDC `sub` / SAML NameID).
    pub subject: String,
}

/// An authenticated principal. Acts as a member of one or more workspaces.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub id: UserId,
    pub email: String,
    pub display_name: String,
    /// Present when the user is backed by SSO; `None` for the dev/seeded admin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sso_subject: Option<Subject>,
}

/// A workspace role. Sets the base capability set; grants (§19) attenuate within
/// it (SOUL §18).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Owner,
    Admin,
    Member,
    Viewer,
}

/// Binds a [`User`] to a [`Workspace`] with a [`Role`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Membership {
    pub workspace_id: WorkspaceId,
    pub user_id: UserId,
    pub role: Role,
}

/// Who authored an object — a human or an agent (SOUL §5, §21).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Author {
    User { id: UserId },
    Agent { id: AgentId },
}

// ---------------------------------------------------------------------------
// Connections, calendars, events
// ---------------------------------------------------------------------------

/// The kind of external system a [`Connection`] talks to. Stays abstract: it
/// names a *category*, not a vendor implementation (SOUL §3.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionKind {
    /// A calendar source (local ics / CalDAV / Google).
    Calendar,
    /// A storage bucket source (local FS / S3 / WebDAV).
    Storage,
    /// An email source (local Maildir / IMAP / JMAP / Gmail). Read-only (§28).
    Email,
    /// A messaging channel (Matrix / Telegram / Discord).
    Channel,
    /// An external PostgreSQL database the workspace owns: catalerum manages its
    /// schema (manual + declarative migrations) and runs capability-gated SQL
    /// against it from tools and automations (SOUL §11/§19). Credentials are
    /// encrypted at rest behind `credential_ref`; per-provider settings (host,
    /// port, database, username, options) ride in the connection `config` blob.
    Postgres,
}

/// A configured link to an external provider, with an encrypted
/// `credential_ref` (never plaintext, SOUL §13) and an opaque sync `cursor`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Connection {
    pub id: ConnectionId,
    pub workspace_id: WorkspaceId,
    pub kind: ConnectionKind,
    pub name: String,
    /// Opaque reference into the secret store (SOUL §13).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_ref: Option<String>,
    /// Last incremental-sync position (sync-token / ETag / sequence).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<Cursor>,
}

/// An opaque, provider-defined incremental-sync position. Matches provider
/// semantics exactly (SOUL §15): a sync-token, an ETag, or a sequence number.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Cursor(pub String);

impl Cursor {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

/// A calendar. Usually belongs to a provider [`Connection`]; a **local**
/// calendar (`connection_id` is `None`) lives entirely in the database — it is
/// not synced from anything external and its events are created/edited directly
/// (SOUL §8/§11). Local calendars are the writable substrate automations target.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Calendar {
    pub id: CalendarId,
    pub workspace_id: WorkspaceId,
    /// The provider connection this calendar belongs to, or `None` for a local
    /// (database-native, read-write) calendar.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<ConnectionId>,
    /// Provider-native identifier for the calendar. For a local calendar this is
    /// a workspace-unique opaque key (the calendar's own id, or a stable slug
    /// like `"default"`), not tied to any provider.
    pub external_id: String,
    pub name: String,
    pub read_only: bool,
}

impl Calendar {
    /// Whether this is a **local** (database-native, no-connection) calendar —
    /// the kind whose events are created/edited directly rather than synced
    /// from a provider (SOUL §8).
    #[must_use]
    pub fn is_local(&self) -> bool {
        self.connection_id.is_none()
    }
}

/// A calendar event. Fields mirror iCalendar / provider semantics (SOUL §8,
/// §15): `uid`, `rrule`, `etag`, `sequence` are kept faithfully.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub id: EventId,
    pub workspace_id: WorkspaceId,
    pub calendar_id: CalendarId,
    /// iCalendar `UID` (stable across edits).
    pub uid: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// Whole-day event (iCalendar `VALUE=DATE` / Google `date` endpoints).
    /// `start`/`end` still carry instants (midnight UTC by convention), but a
    /// set flag marks the event as covering calendar *dates*, so UIs render it
    /// in an all-day strip rather than as a timed block.
    #[serde(default)]
    pub all_day: bool,
    /// RFC 5545 recurrence rule, verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rrule: Option<String>,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    /// Resolved attendees as typed entity pointers.
    #[serde(default)]
    pub attendees: Vec<EntityRef>,
    /// Free-text description / body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Category labels (iCalendar `CATEGORIES`, SOUL §8). Free-text tags;
    /// projected to the derived graph as `:Topic` nodes (§6.3), like note tags.
    #[serde(default)]
    pub labels: Vec<String>,
    /// File / image attachments (iCalendar `ATTACH`, SOUL §8/§9): an uploaded
    /// object in the workspace store or an external link.
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    /// Provider ETag for idempotent incremental sync (SOUL §3.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    /// iCalendar `SEQUENCE` for conflict resolution.
    pub sequence: i64,
}

/// A file or image attached to a calendar [`Event`] (iCalendar `ATTACH`, SOUL
/// §8/§9). Either an uploaded object in the workspace store or an external link
/// (or an `ATTACH` URI synced from a provider); both reduce to a fetchable `url`
/// plus display metadata. An `image/*` [`content_type`](Self::content_type)
/// renders inline; anything else is a download link.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    /// Where the bytes live: a workspace storage path (`/storage/objects/{key}`)
    /// for an uploaded file, or an absolute URL for an external link / synced
    /// `ATTACH` URI.
    pub url: String,
    /// Display filename, if known (iCalendar `FILENAME` / `X-FILENAME` param).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    /// MIME type, if known (iCalendar `FMTTYPE` param). `image/*` renders inline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// Size in bytes, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// A skill invoked directly on a user turn (the composer's `/<skill>` command,
/// SOUL §12/§23): a point-in-time snapshot of the skill's runbook, attached to
/// the [`Message`] like a file attachment. The stored `content` (what the UI
/// shows) stays the short invocation text; the agent loop renders this snapshot
/// into the turn the model sees — on the live turn and on every replay. A
/// snapshot (not a name to re-resolve) so the transcript stays stable if the
/// skill is later edited or deleted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillInvocation {
    /// The skill's per-workspace-unique name.
    pub name: String,
    /// The skill's Markdown runbook at invocation time.
    pub instructions: String,
    /// The tools the skill is meant to use (names), at invocation time.
    #[serde(default)]
    pub tools: Vec<String>,
}

// ---------------------------------------------------------------------------
// Buckets, stored objects
// ---------------------------------------------------------------------------

/// A storage bucket belonging to a [`Connection`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bucket {
    pub id: BucketId,
    pub workspace_id: WorkspaceId,
    pub connection_id: ConnectionId,
    pub name: String,
    /// Optional key prefix this bucket is scoped to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
}

/// Metadata for one object in a [`Bucket`]. The blob itself stays in the bucket
/// (never the DB, SOUL §14); this row is the catalogued, searchable handle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredObject {
    pub id: ObjectId,
    pub workspace_id: WorkspaceId,
    pub bucket_id: BucketId,
    /// Object key within the bucket.
    pub key: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    pub last_modified: DateTime<Utc>,
    /// Content hash for dedup / change detection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// The [`Document`] holding extracted text, once ingested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extracted_text_id: Option<DocumentId>,
}

/// A user- or agent-applied **label** on a stored file or directory (SOUL §9).
///
/// Labels are free-text categories a user (or an automation) attaches to a path
/// in a store's tree — a folder (`is_dir`) or a single file — so the Files panel
/// can tag and filter by them. Unlike a [`StoredObject`], a label is keyed by
/// `(store, path)` rather than by a catalogue id, so it can tag a **directory**
/// (which has no object row) or a file whose bytes exist but isn't catalogued
/// yet. Postgres is the source of truth; `path` is the user-facing key (never the
/// physical `<workspace_id>/…` namespaced one, §18).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectLabel {
    pub id: ObjectLabelId,
    pub workspace_id: WorkspaceId,
    /// The `?store=` selector the labelled path lives on (empty → the default
    /// store). A path is only unambiguous within a store (SOUL §9).
    pub store: String,
    /// The user-facing key (a file's key, or a directory path — no trailing `/`).
    pub path: String,
    /// Whether `path` is a directory (`true`) or a single file (`false`).
    pub is_dir: bool,
    /// The free-text label (e.g. "archive", "invoice", "shared").
    pub label: String,
    /// Who applied the label — a human or an agent (SOUL §5/§21).
    pub author: Author,
    pub created_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Mailboxes, emails (read-only email ingest, SOUL §28)
// ---------------------------------------------------------------------------

/// An RFC 5322 mailbox address with an optional display name — the raw fact a
/// provider parses (`"Ada Lovelace" <ada@example.com>`). Resolution to a Person
/// [`Entity`] is a derived graph step (§6.3/§28), not the provider's job.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmailAddress {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub address: String,
}

impl EmailAddress {
    /// An address with no display name.
    #[must_use]
    pub fn new(address: impl Into<String>) -> Self {
        Self {
            name: None,
            address: address.into(),
        }
    }
}

/// A mailbox (folder) exposed by an email [`Connection`] (SOUL §28). The
/// provider-agnostic analogue of a [`Calendar`]; `external_id` is the
/// provider-native identifier (IMAP folder name, JMAP mailbox id, Maildir path).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mailbox {
    pub id: MailboxId,
    pub workspace_id: WorkspaceId,
    pub connection_id: ConnectionId,
    /// Provider-native identifier for the mailbox.
    pub external_id: String,
    pub name: String,
    pub read_only: bool,
}

/// A normalized email message (SOUL §28). catalerum **reads** mail — it is not a
/// mail client (no send/reply, §14). Synced idempotently by `(mailbox_id, uid)`
/// where `uid` is the provider's stable id (IMAP `UID`, JMAP id, Maildir unique
/// filename). `From`/`To`/`Cc` keep the raw addresses (Postgres truth); the graph
/// resolves them into Person nodes + `SENT_BY`/`ADDRESSED_TO` edges (§6.3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Email {
    pub id: EmailId,
    pub workspace_id: WorkspaceId,
    pub mailbox_id: MailboxId,
    /// Provider-stable id (idempotency key with `mailbox_id`).
    pub uid: String,
    /// RFC 5322 `Message-ID`, when present (for cross-folder dedup, §29).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<EmailAddress>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub to: Vec<EmailAddress>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cc: Vec<EmailAddress>,
    pub subject: String,
    /// The `Date:` header (the message's own timestamp), when parseable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub received_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_html: Option<String>,
    pub has_attachments: bool,
    /// Provider flags (`seen`, `flagged`, `answered`, …), provider-native tokens.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<String>,
    /// Free-text **labels** applied by automations (SOUL §11/§28) — a classifier
    /// verdict recorded by a `LabelEmail` action (e.g. `"receipt"`, `"urgent"`).
    /// Distinct from [`flags`](Self::flags), which are the provider's own tokens;
    /// these are catalerum-side categories the user/agent assigns. Mirrors
    /// [`Event::labels`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// Object-storage key (`mail/<id>.eml`) of the archived raw RFC 5322 message —
    /// the body + every attachment as MIME parts (SOUL §9/§28/§29). `None` until
    /// the message is archived, or when no storage backend is configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_ref: Option<String>,
    /// Archived attachment **references** (SOUL §9/§28/§29): each MIME attachment of
    /// this message, once archived, is a separate object in the workspace's files
    /// store, linked here by an [`Attachment`] (`url` = `/storage/objects/<key>`).
    /// Empty until the message is archived, or when no storage backend is configured.
    /// The bytes live in the bucket and ride the §10 object-ingest pipeline (extract
    /// → chunk → embed) like any file — never inlined as chunks of the email document.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    /// The raw RFC 5322 bytes, carried **transiently** from a provider's `sync` to
    /// the ingest worker so it can archive them to object storage. Never persisted
    /// (it is not a DB column) and never serialized over the wire.
    #[serde(default, skip)]
    pub raw: Option<Vec<u8>>,
}

/// An email attachment **extracted from a raw RFC 5322 message**, carried
/// transiently from `catalerum-email`'s MIME parse to the archival seam (SOUL
/// §9/§28/§29). Unlike a persisted [`Attachment`] (a `url` reference into a store),
/// this holds the decoded bytes in memory just long enough to be written to the
/// bucket; it is never persisted or serialized.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtractedAttachment {
    /// Display filename, if the part declared one (`Content-Disposition: filename`
    /// or `Content-Type: name`).
    pub filename: Option<String>,
    /// MIME type, if known (the part's `Content-Type`).
    pub content_type: Option<String>,
    /// The decoded attachment bytes.
    pub data: Vec<u8>,
}

// ---------------------------------------------------------------------------
// Entities, documents, chunks (the catalogue + retrieval substrate)
// ---------------------------------------------------------------------------

/// The kind of a catalogued [`Entity`] — mirrors the Neo4j node labels
/// (SOUL §6.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    Person,
    Org,
    Topic,
    Project,
    Place,
}

/// A catalogued thing in your world: a person, org, topic, project, or place
/// (SOUL §1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entity {
    pub id: EntityId,
    pub workspace_id: WorkspaceId,
    pub kind: EntityKind,
    pub display_name: String,
    /// Alternate names used for dedup/matching.
    #[serde(default)]
    pub aliases: Vec<String>,
}

/// Extracted text for a source artifact (file, note, message). The unit that
/// gets chunked + embedded (SOUL §10).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Document {
    pub id: DocumentId,
    pub workspace_id: WorkspaceId,
    /// Where this text came from (Postgres truth pointer).
    pub source: SourceRef,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// A slice of a [`Document`], embedded into Qdrant (SOUL §6.4). Rebuildable from
/// Postgres truth; `qdrant_point_id` is the derived index handle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chunk {
    pub id: ChunkId,
    pub workspace_id: WorkspaceId,
    pub document_id: DocumentId,
    /// Position within the document.
    pub ordinal: i32,
    pub text: String,
    /// The point id in the vector index, once upserted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qdrant_point_id: Option<uuid::Uuid>,
}

// ---------------------------------------------------------------------------
// Notes, profile, memories, skills (personalization & knowledge)
// ---------------------------------------------------------------------------

/// A user- or LLM-authored markdown note (SOUL §21).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Note {
    pub id: NoteId,
    pub workspace_id: WorkspaceId,
    pub author: Author,
    pub title: String,
    pub markdown: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub updated_at: DateTime<Utc>,
}

/// A user- or agent-authored **relationship** between two objects (SOUL §5/§6.3).
///
/// The endpoints are [`SourceRef`]s, so a link can connect any two first-class
/// rows — a note to a calendar event, a file to an email, and so on. The link is
/// **directed** (`from → to`): `A → B` is a distinct relationship from `B → A`.
/// `label` is an optional free-text relation kind ("attachment", "follow-up", …)
/// and `note` an optional annotation; neither is constrained to a fixed set.
///
/// Stored in Postgres as the source of truth and projected into the derived Neo4j
/// graph as a `RELATES_TO` edge (rebuildable, SOUL §6.3). Endpoints are *not*
/// foreign-keyed (a `SourceRef` is polymorphic) — a link may outlive the object it
/// points at until reconciled.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    pub id: LinkId,
    pub workspace_id: WorkspaceId,
    /// The relationship's source endpoint.
    pub from: SourceRef,
    /// The relationship's target endpoint.
    pub to: SourceRef,
    /// Optional free-text relation label (e.g. "attachment", "follow-up").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Optional free-text annotation on the relationship.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Who created the link — a human or an agent (SOUL §5/§21).
    pub author: Author,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// An AI-authored **emerged UI** — a declarative component tree the AI can
/// create and edit, rendered inline in chat and reopenable from the Apps panel.
///
/// The structure lives in [`definition`](UiDefinition::definition); transient UI
/// state (current view, open dialogs, in-progress inputs) is client-side and not
/// persisted in v1. `version` is an optimistic edit-concurrency counter (bumped
/// per patch); `spec_version` is the JSONB format version (for future shape
/// migrations). `Eq` is not derived because the spec holds arbitrary JSON.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UiDefinition {
    pub id: UiDefinitionId,
    pub workspace_id: WorkspaceId,
    pub author: Author,
    /// Optional slug, unique-when-set per workspace; meaningful for the Apps panel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSONB format version (append-only enum evolution; see `model_ui`).
    pub spec_version: u32,
    /// Optimistic edit-concurrency counter (distinct from any state version).
    pub version: i64,
    pub definition: UiSpec,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// An unanswered `ask_user` question form (SOUL §7/§12), persisted so it survives a
/// page reload / socket reconnect. The chat LLM's `ask_user` tool creates one tied
/// to the conversation; the client renders it as an interactive form (fetched on
/// load, or pushed live). It is resolved when the user answers (their answer is an
/// ordinary follow-up turn) — at most one is unresolved per conversation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingQuestion {
    pub id: PendingQuestionId,
    pub workspace_id: WorkspaceId,
    pub conversation_id: ConversationId,
    /// The questions to ask (choices + free-text modes; see [`crate::ask::Question`]).
    pub questions: Vec<crate::ask::Question>,
    pub created_at: DateTime<Utc>,
    /// When the user answered (or the question was superseded); `None` while pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<DateTime<Utc>>,
    /// The structured [`Answer`](crate::ask::Answer)s the user gave via the form —
    /// the durable record of what they picked/typed, keyed by question id. `None`
    /// while pending, and stays `None` when the question was superseded (the user
    /// typed past the form instead of answering it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answers: Option<Vec<crate::ask::Answer>>,
}

/// A guarded tool call **deferred** until the user approves or rejects it (SOUL
/// §7/§12/§19), persisted so the prompt survives a page reload / socket reconnect /
/// **server restart** — the tool is held (never run) until a decision lands.
///
/// A profile's tool guard (§19) produces this when a call classifies as
/// "require-user-feedback": instead of blocking the turn, the call is recorded
/// here and the turn ends; the client renders an Approve/Reject prompt (fetched on
/// load, or pushed live). On **approve** the agent re-runs the call (the guard now
/// allows it); on **reject** the guard denies it with the reason. At most one row
/// is unresolved per conversation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingApproval {
    pub id: PendingApprovalId,
    pub workspace_id: WorkspaceId,
    pub conversation_id: ConversationId,
    /// The tool awaiting approval.
    pub tool: String,
    /// Its JSON arguments — matched on the agent's re-attempt so the decision
    /// applies to *this* exact call.
    pub arguments: Json,
    /// Why the guard escalated (the classifier's reason), shown in the prompt.
    pub reason: String,
    pub created_at: DateTime<Utc>,
    /// When the user decided (or the approval was superseded); `None` while pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<DateTime<Utc>>,
    /// The user's decision; `None` while pending or if superseded without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<ApprovalDecision>,
}

/// The user's ruling on a [`PendingApproval`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    /// Run the deferred tool call.
    Approved,
    /// Block it (the guard denies with the reason).
    Rejected,
}

/// A structured per-user personalization record (tz, hours, prefs, relations).
/// Injected into the system prompt every turn (SOUL §22).
///
/// `Eq` is not derived because `fields` holds arbitrary JSON.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub workspace_id: WorkspaceId,
    pub user_id: UserId,
    /// Free-form structured fields (timezone, working hours, preferences, …).
    #[serde(default)]
    pub fields: Map,
}

/// A per-user override of the `[llm]` config model/voice defaults (SOUL §7/§13).
///
/// The `[llm]` TOML block is the immutable boot-time base; this record is the
/// runtime layer a user sets from the workbench (principle 10). Each field is
/// `None` when unset — the effective value then falls back to the corresponding
/// `[llm]` config default. Keyed on `(workspace_id, user_id)` like the
/// [`Profile`], but kept a **separate** record so a model/voice choice never
/// leaks into the chat system prompt (which renders every profile field).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LlmSettings {
    pub workspace_id: WorkspaceId,
    pub user_id: UserId,
    /// Chat / completion model id; `None` → `[llm].default_model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_model: Option<String>,
    /// Text-to-speech model id; `None` → `[llm].speech_model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speech_model: Option<String>,
    /// Text-to-speech voice id; `None` → `[llm].speech_voice`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speech_voice: Option<String>,
    /// Speech-to-text model id; `None` → `[llm].transcription_model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcription_model: Option<String>,
    /// Browser microphone audio is shortened by this factor before STT. A value
    /// of `1.5` uploads two seconds for every three seconds recorded.
    #[serde(default = "default_voice_input_speed")]
    pub voice_input_speed: f32,
    /// OCR vision model id (image → text via the vision engine); `None` → the
    /// configured `[ocr]` engine chain decides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ocr_model: Option<String>,
    /// Model ids this user forces to accept **image** input regardless of what the
    /// gateway catalog advertises (SOUL §7/§9) — the per-user layer of the chat
    /// image-inlining override, unioned with `[llm].image_input_models`. A model
    /// here has an uploaded image sent to it as multimodal content even if the
    /// catalog says it's text-only. Empty = no overrides.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image_input_models: Vec<String>,
}

/// Default microphone time-compression factor for new/legacy settings records.
#[must_use]
pub const fn default_voice_input_speed() -> f32 {
    1.5
}

/// A per-user override of the `[search]` default provider (SOUL §7/§13).
///
/// The `[search]` TOML block sets the boot-time default backend; this record is
/// the runtime layer a user sets from the workbench (principle 10). When unset,
/// the effective default falls back to `[search].backend`. Keyed on
/// `(workspace_id, user_id)` like [`LlmSettings`], and kept a **separate** record
/// for the same reason — a preference must never leak into the chat system prompt.
///
/// Provider **API keys are deliberately not here**: they are billed infrastructure
/// secrets shared by the workspace, so they live only in `[search]` config /
/// environment (SOUL §13). This record holds nothing secret — only an engine name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchSettings {
    pub workspace_id: WorkspaceId,
    pub user_id: UserId,
    /// Preferred default search provider (`brave`/`tavily`/…); `None` →
    /// `[search].backend`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_provider: Option<String>,
}

/// A per-user override of the **default files store** (SOUL §7/§9/§13).
///
/// A storage op that names no `?store=` resolves a destination: the boot-time
/// config default (`[storage]`'s `default` backend, or the sole store) is the
/// base; this record is the runtime layer a user sets from the workbench
/// (principle 10), so an upload "to my files" lands wherever the user chose.
/// When unset, the effective default falls back to the config default. Keyed on
/// `(workspace_id, user_id)` like [`LlmSettings`]/[`SearchSettings`], and kept a
/// **separate** record for the same reason — a preference must never leak into
/// the chat system prompt.
///
/// Holds nothing secret: a store **name**, not credentials. Backend secrets live
/// in `[storage]` config / a `Connection`'s credential ref (SOUL §13).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageSettings {
    pub workspace_id: WorkspaceId,
    pub user_id: UserId,
    /// Preferred default store name (the `?store=` selector value); `None` → the
    /// `[storage]` config default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_store: Option<String>,
}

/// Whether a memory is private to a user or shared across the workspace
/// (SOUL §22).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    User,
    Workspace,
}

/// A durable, free-text fact auto-curated during conversations/automations and
/// recalled semantically (SOUL §22). Always an inspectable, editable row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Memory {
    pub id: MemoryId,
    pub workspace_id: WorkspaceId,
    pub scope: MemoryScope,
    /// When `scope == User`, the user this memory belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<UserId>,
    pub text: String,
    /// Where the memory was derived from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceRef>,
    /// Vector-index point id, once embedded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub point_id: Option<uuid::Uuid>,
    pub created_at: DateTime<Utc>,
}

/// Optional executable code attached to a [`Skill`], run via the
/// [`Executor`](crate::provider::Executor) (SOUL §20, §23).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Code {
    /// Language identifier (e.g. `python`), matching `exec:run@bao{lang=…}`.
    pub language: String,
    /// The source to execute.
    pub source: String,
    /// Optional pinned entrypoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<String>,
}

/// A reusable, named capability bundle: instructions + restricted tools +
/// optional code (SOUL §23). Invoking it is capability-gated (`skill:use@<name>`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skill {
    pub id: SkillId,
    pub workspace_id: WorkspaceId,
    pub name: String,
    pub description: String,
    /// Markdown runbook / instructions.
    pub instructions_md: String,
    /// Names of tools this skill is allowed to use (subset of the registry).
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<Code>,
    /// Whether the skill's name + description are advertised to the chat agent
    /// in its system prompt ("visible to agent"). On by default; a per-skill
    /// opt-out for skills that should only run when explicitly invoked.
    #[serde(default = "default_true")]
    pub advertised: bool,
}

/// A user-authored, Boa-scripted **MCP endpoint** (SOUL §26): a stored JavaScript
/// program that declares MCP tools and, on a `tools/call`, reaches a narrow host
/// bridge (e.g. `catalerum.callTool("search_semantic", …)`) whose scope is pinned
/// to this endpoint. Served over its own `POST /mcp/e/{name}` (workspace token) and
/// `POST /mcp/s/{token}` (a signed, shareable scoped token), isolated from the
/// main tool surface — an external agent connecting to it sees only the tools the
/// script declares.
///
/// The `bucket_name` + `key_prefix` scope is **injected by the host** into every
/// `search_semantic` call the script makes (a script cannot widen it), so an
/// endpoint over a wiki's subdir can only ever search that subdir — enforced at the
/// bridge + the Qdrant filter, never via capability constraints (which have a known
/// attenuation gap).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpEndpoint {
    pub id: McpEndpointId,
    pub workspace_id: WorkspaceId,
    /// URL-safe slug, unique per workspace — the `{name}` path segment.
    pub name: String,
    pub description: String,
    /// The Boa (JavaScript) program declaring the endpoint's tools + their handlers.
    pub script: String,
    /// The storage bucket the endpoint's search is pinned to (`None` = any bucket
    /// in the workspace).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bucket_name: Option<String>,
    /// The key prefix (subdir) the endpoint's search is pinned to (`None` = the
    /// whole workspace). Injected into every `search_semantic` call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_prefix: Option<String>,
    /// The §19 grant the script's tool calls run under; `None` falls back to a
    /// minimal read-only authority resolved at serve time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant_id: Option<GrantId>,
    /// A disabled endpoint 404s at serve time but is kept for editing.
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub author: Author,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// serde default for a `bool` field that should default to `true`.
fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
// Boards, columns, tasks (Kanban)
// ---------------------------------------------------------------------------

/// A Kanban board with ordered columns (SOUL §24).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Board {
    pub id: BoardId,
    pub workspace_id: WorkspaceId,
    pub name: String,
    pub columns: Vec<Column>,
}

/// An ordered column within a [`Board`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Column {
    pub id: ColumnId,
    pub name: String,
    /// Sort position among the board's columns.
    pub order: i32,
}

/// Lifecycle state of a [`Task`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Open,
    InProgress,
    Blocked,
    Done,
}

/// A Kanban task, worked one-by-one by agents within their grant (SOUL §24).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub workspace_id: WorkspaceId,
    pub board_id: BoardId,
    pub column_id: ColumnId,
    pub title: String,
    #[serde(default)]
    pub body_md: String,
    /// User or agent the task is assigned to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<Author>,
    /// Sort position within the column.
    pub order: i32,
    pub status: TaskStatus,
}

// ---------------------------------------------------------------------------
// Channels & conversations
// ---------------------------------------------------------------------------

/// A messaging integration kind (SOUL §25).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    Matrix,
    Telegram,
    Discord,
}

/// A configured messaging channel; credentials are an encrypted `config_ref`
/// (SOUL §25).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Channel {
    pub id: ChannelId,
    pub workspace_id: WorkspaceId,
    pub kind: ChannelKind,
    /// Opaque reference into the secret store for this channel's config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_ref: Option<String>,
}

/// Where a [`Conversation`] originated (SOUL §5, §25).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// The web workbench chat panel.
    Web,
    /// A new chat thread created by an automation output action.
    Automation,
    /// An inbound messenger channel.
    Channel,
    /// An external MCP client.
    Mcp,
}

/// A chat thread.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conversation {
    pub id: ConversationId,
    pub workspace_id: WorkspaceId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Free-text topic tags, generated by the background chat-metadata pass
    /// (auto title + tag) and rendered as pills under the sidebar title. An
    /// explicit rename never touches these; the generator owns them.
    #[serde(default)]
    pub tags: Vec<String>,
    /// `true` iff the title was set by an explicit user rename
    /// (`PUT /conversations/{id}`) — the background auto-title pass must not
    /// overwrite a human-chosen name.
    #[serde(default)]
    pub title_manual: bool,
    pub origin: Origin,
    /// The [`AgentProfile`] this thread runs as, if bound via the chat picker
    /// (SOUL §19): the chat loop uses its model/prompt/tools under the user's own
    /// authority (never escalating). `None` = the default chat (user's role).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_profile_id: Option<AgentProfileId>,
    /// The model this thread's chat loop thinks with, if pinned via the chat
    /// "model" picker (SOUL §7/§12). The most specific per-thread choice, so the
    /// ws handler lets it win over a bound profile's model and the user/workspace
    /// default. A free-form gateway model id (like `llm_settings.chat_model`);
    /// `None` = no override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The reasoning ("thinking") effort this thread's chat loop requests, if set
    /// via the chat "thinking" picker (SOUL §7/§12): a free-form gateway effort
    /// token (`"low" | "medium" | "high" | "xhigh" | "max"`) passed through to the
    /// model. `None` = no reasoning requested (the provider default). Persisted
    /// per-thread like [`model`](Self::model).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Rolling auto-compaction summary of the transcript prefix (SOUL §7/§12):
    /// when the replayed history approaches the model's context window, a
    /// background pass folds the older messages into this summary, and the next
    /// turn seeds `[summary] + messages after summary_upto` instead of the whole
    /// transcript. Only meaningful when **both** this and
    /// [`summary_upto`](Self::summary_upto) are set; messages are never deleted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// The last [`Message`] the [`summary`](Self::summary) covers — the seed
    /// replays messages strictly after it. Nulled by the store when that row is
    /// deleted (a regenerate pruning the tail), which invalidates the summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_upto: Option<MessageId>,
    /// When the thread was started (newest-first list + time grouping, SOUL §12).
    pub created_at: DateTime<Utc>,
}

/// The role of a [`Message`] in an LLM conversation (OpenAI/OpenRouter shape,
/// SOUL §7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

/// A request to invoke a tool, as emitted by the model (assembled from streamed
/// deltas, SOUL §7). Mirrors the OpenRouter `tool_calls` shape.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Provider-assigned call id, echoed back on the tool result message.
    pub id: String,
    /// Tool/function name to dispatch in the [`ToolRegistry`](crate::tool::ToolRegistry).
    pub name: String,
    /// JSON-encoded arguments (kept as a string to match the wire shape; parse
    /// before dispatch).
    pub arguments: String,
}

/// A single message in a [`Conversation`].
///
/// Not `Eq`: [`usage`](Self::usage) carries an `f64` cost.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub id: MessageId,
    pub conversation_id: ConversationId,
    pub role: MessageRole,
    pub content: String,
    /// File / image references attached to a **user** turn (SOUL §9/§12): an
    /// uploaded object in the workspace store or an external link — the same
    /// [`Attachment`] shape calendar events use. The bytes are NOT embedded in
    /// `content`; the agent loop renders these into the turn as references the
    /// model can `stage_object`/`copy_object`/`read_object`. Empty otherwise.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    /// A `/<skill>` invocation snapshot on a **user** turn (SOUL §12/§23): the
    /// runbook the agent loop attaches to this message for the model. The UI
    /// shows only `content`. `None` for every other row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<SkillInvocation>,
    /// Tool calls emitted by an assistant turn (empty otherwise).
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    /// For a `Tool` message, the id of the tool call it answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// For a `Tool` message, whether the call failed (the `content` holds the
    /// error payload). Always `false` for non-tool rows (SOUL §12).
    #[serde(default)]
    pub tool_is_error: bool,
    /// For a `Tool` message, the dispatch duration in milliseconds, when measured.
    /// `None` for non-tool rows and turns recorded before this was captured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_duration_ms: Option<i64>,
    /// Per-turn token + cache + cost accounting for the exchange this message
    /// concludes — the agent loop's summed usage, recorded on the **final
    /// assistant message** of the exchange. Drives the persisted token info-icon
    /// (and cost readout) on a replayed transcript, so a reopened conversation
    /// shows the same accounting the live turn did. `None` for user/tool/system
    /// rows, non-final assistant turns, turns where usage was not reported, and
    /// transcripts recorded before this was captured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<crate::stream::Usage>,
    pub created_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Authorization (grants/agents); see `crate::capability` for the model.
// ---------------------------------------------------------------------------

/// A named capability bundle with global constraints (SOUL §19). The
/// capabilities themselves are [`Capability`](crate::capability::Capability);
/// see [`crate::capability`] for matching/attenuation.
///
/// `Eq` is not derived (capabilities/constraints may carry JSON floats).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Grant {
    pub id: GrantId,
    pub workspace_id: WorkspaceId,
    pub name: String,
    /// The capabilities this grant confers.
    #[serde(default)]
    pub capabilities: Vec<crate::capability::Capability>,
    /// Global constraints (env allow-list, rate/cost caps, time window,
    /// dry-run, per-action approval) (SOUL §19).
    #[serde(default)]
    pub constraints: crate::capability::Constraints,
}

/// Which [`Executor`](crate::provider::Executor) backend an agent's commands run
/// on (SOUL §20). Names categories, not implementations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutorKind {
    /// Host process. Highest blast radius; protected, opt-in (SOUL §20).
    Local,
    /// Host process in a locked-down working directory with a scrubbed
    /// environment and no-network posture — lightweight isolation without a
    /// container (SOUL §20).
    Sandbox,
    /// Ephemeral container per command/session. Default sandbox.
    Container,
    /// Short-lived Kubernetes Jobs / ephemeral Pods.
    Kubernetes,
    /// The bao secure native-code sandbox.
    Bao,
}

impl ExecutorKind {
    /// The stable snake_case token (matches the serde form + the DB `backend`
    /// column), reusable by config parsing and the store layer.
    #[must_use]
    pub fn as_token(&self) -> &'static str {
        match self {
            ExecutorKind::Local => "local",
            ExecutorKind::Sandbox => "sandbox",
            ExecutorKind::Container => "container",
            ExecutorKind::Kubernetes => "kubernetes",
            ExecutorKind::Bao => "bao",
        }
    }

    /// Parse a backend token (the inverse of [`ExecutorKind::as_token`]).
    /// `container` accepts `podman`/`docker` and `kubernetes` accepts `k8s` as
    /// friendly aliases. Returns `None` for an unknown token.
    #[must_use]
    pub fn parse_token(token: &str) -> Option<Self> {
        match token.trim().to_ascii_lowercase().as_str() {
            "local" => Some(ExecutorKind::Local),
            "sandbox" => Some(ExecutorKind::Sandbox),
            "container" | "podman" | "docker" => Some(ExecutorKind::Container),
            "kubernetes" | "k8s" => Some(ExecutorKind::Kubernetes),
            "bao" => Some(ExecutorKind::Bao),
            _ => None,
        }
    }
}

/// Lifecycle state of a [`TerminalSession`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalSessionStatus {
    /// Live — a PTY / process is attached on some node.
    Active,
    /// Closed cleanly.
    Closed,
    /// Ended in error.
    Failed,
}

/// The durable record of one interactive terminal an agent stood up (SOUL §20).
/// The live PTY / process is node-local and tracked by the API's terminal
/// manager; this row is only the persisted lifecycle. Every terminal runs in a
/// throwaway ephemeral working directory, synced to object storage on demand.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSession {
    pub id: TerminalSessionId,
    pub workspace_id: WorkspaceId,
    /// Executor runtime the session runs on.
    pub backend: ExecutorKind,
    pub status: TerminalSessionStatus,
    /// Where the session's files live on disk (for the ephemeral flush).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_dir: Option<String>,
    /// Last object-storage key prefix this session was persisted under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_prefix: Option<String>,
    /// The pod (process) that owns this session's node-local PTY (multi-pod HA,
    /// SOUL §16 M7). `None` for a legacy/pre-upgrade row. Only the owning pod can
    /// drive the session; boot reconcile reclaims only its own (+ NULL) rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_id: Option<String>,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<DateTime<Utc>>,
}

/// Lifecycle state of a [`WorkspaceSandboxRecord`] (SOUL §20).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxState {
    /// Declared but not yet running (or being provisioned).
    Pending,
    /// Live — a container/Pod is running and ready to exec into.
    Ready,
    /// Provisioning failed.
    Failed,
    /// Created but not running (idle-reaped / suspended).
    Stopped,
}

/// The durable record of a workspace's per-workspace sandbox (SOUL §20). Exactly
/// one per workspace (`workspace_id` is the primary key). The live container/Pod
/// is node-local (tracked by the API's sandbox manager / the in-cluster
/// operator); this row is only the persisted desired + observed state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSandboxRecord {
    pub workspace_id: WorkspaceId,
    /// Executor runtime: [`Container`](ExecutorKind::Container) or
    /// [`Kubernetes`](ExecutorKind::Kubernetes).
    pub backend: ExecutorKind,
    /// The image the sandbox runs.
    pub image: String,
    pub status: SandboxState,
    /// Backend reference (container/Pod name) once provisioned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_ref: Option<String>,
    /// Persistent `/work` volume / PVC name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume_ref: Option<String>,
    /// The pod (process) that last provisioned this sandbox's node-local
    /// container/Pod (multi-pod HA, SOUL §16 M7). `None` for a legacy row. Boot
    /// reconcile marks only its own (+ NULL) rows stopped, never a peer pod's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_id: Option<String>,
    pub last_activity: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A scoped agent: prompt + allowed tools + skills + a [`Grant`] + an executor
/// backend (SOUL §19). Provably ⊆ its creator's authority (attenuation).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Agent {
    pub id: AgentId,
    pub workspace_id: WorkspaceId,
    pub name: String,
    pub system_prompt: String,
    /// Tool names the agent may dispatch (subset of the registry).
    #[serde(default)]
    pub tools: Vec<String>,
    /// Skills the agent may invoke.
    #[serde(default)]
    pub skills: Vec<SkillId>,
    /// The grant authorizing the agent.
    pub grant_id: GrantId,
    /// Execution backend for `run_command` / skill code.
    pub executor: ExecutorKind,
}

/// A persisted, named **agent profile** (SOUL §19): a reusable scoped-agent
/// configuration that bundles a model choice, a system prompt, an allowed tool /
/// skill set, the **subagents** it may delegate to, the **channels** it listens
/// on, and the [`Grant`] that is its authority — all within one
/// [`workspace`](Workspace) (the tenancy + data boundary, §18).
///
/// A profile is the durable form of the §19 [`Agent`]: where `Agent` is the
/// in-flight scoped agent, an `AgentProfile` is a stored configuration a user
/// stands up from the UI/API, binds to channels, and reuses. It exists so that
/// *separate, securely-scoped data access* is a first-class object: each profile
/// can hold a different, attenuated grant, so a "calendar bot" profile literally
/// cannot read storage, and a parent profile can only delegate to a subagent
/// whose grant is ⊆ its own (the attenuation invariant, [`attenuate`]).
///
/// Names rather than ids reference the tool/skill/subagent/channel/terminal sets, matching
/// the rest of the system (skills, automation `LlmAgent`, channel inbound routing
/// are all name-keyed within a workspace); an empty `tools` list means "advertise
/// the whole registry" (the grant still bounds the agent).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentProfile {
    pub id: AgentProfileId,
    pub workspace_id: WorkspaceId,
    /// Unique (per workspace) profile name.
    pub name: String,
    /// The model this profile runs against; `None` uses the workspace default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The profile's system prompt; `None` uses the default agent system prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// Tool names the profile may dispatch (subset of the registry); empty = all.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Skill names the profile may invoke (their runbooks seed the system prompt).
    #[serde(default)]
    pub skills: Vec<String>,
    /// Names of other [`AgentProfile`]s this profile may delegate to via the
    /// `delegate` tool. A subagent runs under its **own** grant, enforced ⊆ this
    /// profile's grant at delegation time (attenuation, §19).
    #[serde(default)]
    pub subagents: Vec<String>,
    /// Channel names this profile listens on: an inbound message on one routes to
    /// this profile's agent loop, which replies on that channel (SOUL §25).
    #[serde(default)]
    pub channels: Vec<String>,
    /// The §19 [`Grant`] that is this profile's authority. `None` runs under the
    /// workspace's bounded base-Member capabilities (the interim default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant_id: Option<GrantId>,
    /// An optional **tool guard**: a programmable classifier (Boa JS and/or LLM)
    /// consulted for every tool call this profile makes, layered *on top of* the
    /// capability grant (SOUL §19). `None` (the default) leaves the profile gated
    /// only by its capabilities. Because subagents are profiles, this covers
    /// delegated runs too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guard: Option<ToolGuard>,
}

/// A per-profile **tool guard** (SOUL §19): a programmable second gate that
/// classifies every tool call — and its output — as allow / deny / require-user-
/// feedback, on top of the static capability check. Enforced at
/// [`ToolRegistry::dispatch`](crate::tool::ToolRegistry::dispatch) via a
/// [`ToolGate`](crate::tool::ToolGate) built from this config.
///
/// Evaluation order: a `script`, if present, decides (it may itself call the LLM
/// via `catalerum.classifyWithLlm`, defaulting to [`llm`](Self::llm)); otherwise
/// a present [`llm`](Self::llm) is a standalone judge; with neither, the guard is
/// inert (every call allowed). A classifier error or an unrecognized decision
/// resolves to [`on_error`](Self::on_error).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolGuard {
    /// A Boa JS classifier: a function body (the code-node calling convention)
    /// receiving a bound `input` describing the call and returning a decision —
    /// `"allow"`/`"deny"`/`"ask"` or `{ decision, reason? }`. May call
    /// `catalerum.callTool(name, args)` and `catalerum.classifyWithLlm(req)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    /// A declarative LLM classifier. Used as a standalone judge when there is no
    /// [`script`](Self::script), and as the default model/instruction backing
    /// `catalerum.classifyWithLlm` when there is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm: Option<ToolGuardLlm>,
    /// A declarative **object-label** policy (SOUL §9/§19): allow/deny a tool call
    /// by the labels on the file (or directory) it touches — e.g. "only files
    /// labelled `shared` are allowed" / "block anything labelled `confidential`".
    /// Applied *before* the script/LLM classifier, and only to calls that reference
    /// an object (a `key`/`path` arg); the object's labels are also surfaced to the
    /// classifier's `input`. `None` (the default) applies no label policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_labels: Option<ObjectLabelPolicy>,
    /// What to do when the classifier errors, times out, or returns something
    /// unrecognized. Defaults to [`GuardFail::Deny`] (fail-closed).
    #[serde(default)]
    pub on_error: GuardFail,
}

/// A declarative object-label allow/deny policy for a [`ToolGuard`] (SOUL §9/§19).
///
/// Evaluated for a tool call that references a stored object (a `key`/`path` arg):
/// the object's [`ObjectLabel`]s are looked up, then **`deny` wins over
/// `require_any`** — a call touching an object with any blocked label is denied,
/// and if `require_any` is non-empty the object must carry at least one of those
/// labels (so "only labelled files are allowed", and an *unlabelled* file is
/// denied). A call that references no object is unaffected.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectLabelPolicy {
    /// When non-empty, the object a call touches must carry **at least one** of
    /// these labels — else the call is denied ("only files with these labels are
    /// allowed", unlabelled files included in the deny).
    #[serde(default)]
    pub require_any: Vec<String>,
    /// An object carrying **any** of these labels is denied (a hard block that
    /// wins over [`require_any`](Self::require_any)).
    #[serde(default)]
    pub deny: Vec<String>,
}

impl ObjectLabelPolicy {
    /// True when the policy imposes no constraint (both lists empty) — an inert
    /// policy the guard can drop.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.require_any.is_empty() && self.deny.is_empty()
    }

    /// Check `labels` (an object's labels) against the policy. Returns
    /// `Some(reason)` when the call must be **denied** (a blocked label is present,
    /// or a required label is missing), else `None` (allowed). `deny` wins.
    #[must_use]
    pub fn violation(&self, labels: &[String]) -> Option<String> {
        if let Some(blocked) = self.deny.iter().find(|d| labels.iter().any(|l| l == *d)) {
            return Some(format!("the object carries a blocked label `{blocked}`"));
        }
        if !self.require_any.is_empty()
            && !self
                .require_any
                .iter()
                .any(|r| labels.iter().any(|l| l == r))
        {
            return Some(format!(
                "the object lacks a required label (one of: {})",
                self.require_any.join(", ")
            ));
        }
        None
    }
}

/// The declarative LLM classifier of a [`ToolGuard`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolGuardLlm {
    /// The model to judge with; `None` uses the profile's model, then the
    /// workspace default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The judge's standing instruction (system prompt): how to decide
    /// allow / deny / require-user-feedback for a described tool call.
    pub instruction: String,
}

/// The fallback ruling when a [`ToolGuard`]'s classifier can't produce a clean
/// decision (error / timeout / unparseable output).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardFail {
    /// Block the call (fail-closed). The default.
    #[default]
    Deny,
    /// Let the call proceed (fail-open).
    Allow,
    /// Escalate to the user (a human present approves/rejects; none → deny).
    Ask,
}

/// A persisted external MCP server connection, managed at runtime (SOUL §26).
///
/// The durable, DB-backed form of a `[[mcp.servers]]` config entry: catalerum
/// connects to it as a **client** (stdio or HTTP/SSE) and folds its tools into the
/// §7 registry as `{name}_{tool}`, each gated on `mcp:use@{name}` (§19). Created /
/// edited / deleted at runtime by the `*_mcp_server` tools, then hot-(dis)connected
/// without a restart. Per workspace (the §18 boundary); `(workspace_id, name)` is
/// unique.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpServerDef {
    pub id: McpServerId,
    pub workspace_id: WorkspaceId,
    /// Unique (per workspace) name; prefixes the server's tools and scopes the
    /// `mcp:use@{name}` capability.
    pub name: String,
    /// `"stdio"` (spawn `command`) or `"http"` (connect to `url`).
    pub transport: String,
    /// Program to spawn (stdio transport).
    #[serde(default)]
    pub command: String,
    /// Arguments to `command` (stdio transport).
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment for the child process (stdio transport).
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    /// Endpoint URL (http transport).
    #[serde(default)]
    pub url: String,
    /// How to authenticate (http transport).
    #[serde(default)]
    pub auth: McpAuthSpec,
    /// Whether to connect this server.
    pub enabled: bool,
    /// Optional allow-list of remote tool names to import; empty → import all.
    #[serde(default)]
    pub tools: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl McpServerDef {
    /// Whether this transport is HTTP-flavoured, else stdio.
    #[must_use]
    pub fn is_http(&self) -> bool {
        matches!(
            self.transport.trim().to_ascii_lowercase().as_str(),
            "http" | "https" | "sse" | "streamable-http"
        )
    }
}

/// How an HTTP [`McpServerDef`] authenticates (SOUL §26). `kind` selects the mode;
/// only that mode's fields are read. **Secrets are stored verbatim** (a follow-up
/// will move them behind the §13 secret store); redact before showing a user.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpAuthSpec {
    /// `none` (default) | `bearer` | `header` | `oauth2`.
    #[serde(default)]
    pub kind: String,
    /// Bearer token (`kind = "bearer"`).
    #[serde(default)]
    pub token: String,
    /// Header name (`kind = "header"`).
    #[serde(default)]
    pub header_name: String,
    /// Header value (`kind = "header"`).
    #[serde(default)]
    pub header_value: String,
    /// Token endpoint (`kind = "oauth2"`).
    #[serde(default)]
    pub token_url: String,
    /// OAuth2 grant: `client_credentials` (default) | `refresh_token`.
    #[serde(default)]
    pub grant_type: String,
    /// OAuth2 client id.
    #[serde(default)]
    pub client_id: String,
    /// OAuth2 client secret.
    #[serde(default)]
    pub client_secret: String,
    /// OAuth2 refresh token (`grant_type = "refresh_token"`).
    #[serde(default)]
    pub refresh_token: String,
    /// OAuth2 scopes, space-separated.
    #[serde(default)]
    pub scope: String,
}

impl McpAuthSpec {
    /// Whether a given secret field is set (used to decide what to redact).
    #[must_use]
    pub fn has_secret(&self) -> bool {
        !self.token.is_empty()
            || !self.header_value.is_empty()
            || !self.client_secret.is_empty()
            || !self.refresh_token.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Automations
// ---------------------------------------------------------------------------

/// A durable trigger→condition→action automation (SOUL §11). The `triggers`,
/// `condition`, and `actions` are kept as structured JSON here in core; the
/// `catalerum-automation` crate owns their concrete typed engine.
///
/// `Eq` is not derived because the trigger/action specs are arbitrary JSON.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Automation {
    pub id: AutomationId,
    pub workspace_id: WorkspaceId,
    pub name: String,
    pub enabled: bool,
    /// Trigger definitions (CalendarEvent / Schedule / Webhook / …).
    #[serde(default)]
    pub triggers: Vec<Json>,
    /// Optional predicate over store/graph/vectors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<Json>,
    /// Ordered typed actions (also the LLM's tools, SOUL §11).
    #[serde(default)]
    pub actions: Vec<Json>,
    /// The full original specification (authoring source of truth).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<Json>,
    /// The grant the automation runs under (SOUL §11, §19).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant_id: Option<GrantId>,
}

/// The lifecycle status of an [`AutomationRun`] (SOUL §11). A run is born
/// `Running`; the other three states are terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Executing (or recovering after a crash, pending reconciliation).
    Running,
    /// Every step completed successfully.
    Succeeded,
    /// A step failed (or the run errored); see `error`.
    Failed,
    /// Cancelled before completion.
    Cancelled,
}

impl RunStatus {
    /// Whether the run has finished (any non-`Running` state).
    #[must_use]
    pub fn is_terminal(self) -> bool {
        !matches!(self, RunStatus::Running)
    }
}

/// The lifecycle status of an [`AutomationStep`] (SOUL §11). A step is born
/// `Running`; the other three states are terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    /// Executing.
    Running,
    /// Completed successfully.
    Succeeded,
    /// Failed; see `error`.
    Failed,
    /// Skipped (e.g. a condition excluded it).
    Skipped,
}

impl StepStatus {
    /// Whether the step has finished (any non-`Running` state).
    #[must_use]
    pub fn is_terminal(self) -> bool {
        !matches!(self, StepStatus::Running)
    }
}

/// One execution of an [`Automation`] (SOUL §11): created when the engine fires a
/// matched trigger, finalized when its ordered actions ([`AutomationStep`]s)
/// complete. The durable audit trail behind §11's "durable run/step state".
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AutomationRun {
    pub id: AutomationRunId,
    pub workspace_id: WorkspaceId,
    pub automation_id: AutomationId,
    pub status: RunStatus,
    /// The §19 grant this run executed under, snapshotted at start — the audit
    /// fact "which grant authorized this run". `None` for a run under default base
    /// authority. Immutable: survives the grant's later deletion (no FK), so it
    /// always reflects what was in force when the run fired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant_id: Option<GrantId>,
    /// What fired the run — the matched trigger + event payload (JSON).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<Json>,
    /// Failure detail when `status` is `Failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub started_at: DateTime<Utc>,
    /// When the run reached a terminal state (`None` while `Running`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
}

/// One action within an [`AutomationRun`] (SOUL §11): the executed action spec,
/// its outcome, and any output. Ordered by `ordinal` within the run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AutomationStep {
    pub id: AutomationStepId,
    pub run_id: AutomationRunId,
    pub workspace_id: WorkspaceId,
    /// Position within the run (0-based, ascending execution order).
    pub ordinal: i32,
    /// The action spec executed (a §11 typed action as JSON).
    pub action: Json,
    pub status: StepStatus,
    /// The action's result, when it produced one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Json>,
    /// Failure detail when `status` is `Failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub started_at: DateTime<Utc>,
    /// When the step reached a terminal state (`None` while `Running`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_profile_guard_round_trips_and_defaults_are_backward_compatible() {
        // A legacy profile with no `guard` key still decodes (field is optional).
        let legacy: AgentProfile = serde_json::from_value(serde_json::json!({
            "id": AgentProfileId::new(),
            "workspace_id": WorkspaceId::new(),
            "name": "legacy",
        }))
        .expect("a profile without a guard still decodes");
        assert!(legacy.guard.is_none());

        // A full guard round-trips, with `on_error` serializing snake_case and
        // defaulting to fail-closed `deny`.
        let profile = AgentProfile {
            id: AgentProfileId::new(),
            workspace_id: WorkspaceId::new(),
            name: "guarded".into(),
            model: None,
            system_prompt: None,
            tools: vec![],
            skills: vec![],
            subagents: vec![],
            channels: vec![],
            grant_id: None,
            guard: Some(ToolGuard {
                script: Some("return 'deny';".into()),
                llm: Some(ToolGuardLlm {
                    model: None,
                    instruction: "Deny writes to prod.".into(),
                }),
                object_labels: Some(ObjectLabelPolicy {
                    require_any: vec!["shared".into()],
                    deny: vec!["confidential".into()],
                }),
                on_error: GuardFail::Ask,
            }),
        };
        let json = serde_json::to_value(&profile).unwrap();
        assert_eq!(json["guard"]["on_error"], serde_json::json!("ask"));
        assert_eq!(json["guard"]["script"], serde_json::json!("return 'deny';"));
        assert_eq!(
            json["guard"]["object_labels"]["deny"][0],
            serde_json::json!("confidential")
        );
        let back: AgentProfile = serde_json::from_value(json).unwrap();
        assert_eq!(back, profile);

        // `on_error` defaults to `deny` when the key is absent, and object_labels is
        // absent by default.
        let g: ToolGuard = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(g.on_error, GuardFail::Deny);
        assert!(g.object_labels.is_none());
    }

    #[test]
    fn object_label_policy_deny_wins_and_require_any_blocks_unlabelled() {
        // A blocked label present → deny (even if a required label is also present).
        let p = ObjectLabelPolicy {
            require_any: vec!["shared".into()],
            deny: vec!["secret".into()],
        };
        assert!(p.violation(&["shared".into(), "secret".into()]).is_some());
        // Required label present, no blocked label → allowed.
        assert!(p.violation(&["shared".into()]).is_none());
        // Neither → denied (lacks a required label).
        assert!(p.violation(&["misc".into()]).is_some());
        // Unlabelled object with a require_any policy → denied.
        assert!(p.violation(&[]).is_some());
        // Deny-only policy: an unlabelled / unrelated object is allowed.
        let d = ObjectLabelPolicy {
            require_any: vec![],
            deny: vec!["secret".into()],
        };
        assert!(d.violation(&[]).is_none());
        assert!(d.violation(&["secret".into()]).is_some());
        // Empty policy is inert.
        assert!(ObjectLabelPolicy::default().is_empty());
    }

    #[test]
    fn link_round_trips_and_tags_endpoints_by_kind() {
        let link = Link {
            id: LinkId::new(),
            workspace_id: WorkspaceId::new(),
            from: SourceRef::Note { id: NoteId::new() },
            to: SourceRef::Event { id: EventId::new() },
            label: Some("follow-up".into()),
            note: None,
            author: Author::User { id: UserId::new() },
            created_at: DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            updated_at: DateTime::from_timestamp(1_700_000_050, 0).unwrap(),
        };
        let json = serde_json::to_value(&link).unwrap();
        // Endpoints are the tagged `SourceRef` shape (`kind` discriminator).
        assert_eq!(json["from"]["kind"], serde_json::json!("note"));
        assert_eq!(json["to"]["kind"], serde_json::json!("event"));
        // Absent `note` is omitted, not serialized as null.
        assert!(json.get("note").is_none());
        assert_eq!(serde_json::from_value::<Link>(json).unwrap(), link);
    }
}
