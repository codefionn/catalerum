//! The graph taxonomy (SOUL §6.3): node labels, edge types, and stable node
//! references.
//!
//! Cypher labels and relationship types **cannot** be parameterized — they are
//! interpolated into the query string. So both are modelled as **closed enums**
//! ([`NodeLabel`], [`EdgeType`]) whose only string forms come from a fixed
//! `&'static str` table: user-derived data never reaches a label position, so
//! there is no Cypher-injection surface. Every other value (workspace id, node
//! id, properties) rides as a `$parameter`.

use catalerum_core::{CalendarId, EntityId, EntityKind, EventId, NoteId, SourceRef, WorkspaceId};

/// A node label in the derived graph. The closed set from SOUL §6.3 — the
/// `Person…Place` entries mirror [`EntityKind`], the rest are first-class
/// catalerum rows projected as nodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NodeLabel {
    Person,
    Org,
    Topic,
    Project,
    Place,
    Event,
    File,
    Note,
    Task,
    Conversation,
    Calendar,
    Bucket,
    Email,
    Memory,
    Document,
    Message,
}

impl NodeLabel {
    /// The exact Cypher label string. Safe to interpolate: it comes only from
    /// this fixed table, never from caller input.
    #[must_use]
    pub const fn as_cypher(self) -> &'static str {
        match self {
            Self::Person => "Person",
            Self::Org => "Org",
            Self::Topic => "Topic",
            Self::Project => "Project",
            Self::Place => "Place",
            Self::Event => "Event",
            Self::File => "File",
            Self::Note => "Note",
            Self::Task => "Task",
            Self::Conversation => "Conversation",
            Self::Calendar => "Calendar",
            Self::Bucket => "Bucket",
            Self::Email => "Email",
            Self::Memory => "Memory",
            Self::Document => "Document",
            Self::Message => "Message",
        }
    }

    /// The node label an [`EntityKind`] projects to (SOUL §5/§6.3).
    #[must_use]
    pub const fn from_entity_kind(kind: EntityKind) -> Self {
        match kind {
            EntityKind::Person => Self::Person,
            EntityKind::Org => Self::Org,
            EntityKind::Topic => Self::Topic,
            EntityKind::Project => Self::Project,
            EntityKind::Place => Self::Place,
        }
    }
}

/// A relationship type in the derived graph — the closed edge set from SOUL §6.3.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EdgeType {
    Attends,
    About,
    Mentions,
    StoredIn,
    ScheduledIn,
    Follows,
    RelatesTo,
    DerivedFrom,
    /// A Note/Task points at an entity/topic it references (SOUL §6.3/§21).
    References,
}

impl EdgeType {
    /// The exact Cypher relationship-type string (UPPER_SNAKE). Safe to
    /// interpolate: closed table, never caller input.
    #[must_use]
    pub const fn as_cypher(self) -> &'static str {
        match self {
            Self::Attends => "ATTENDS",
            Self::About => "ABOUT",
            Self::Mentions => "MENTIONS",
            Self::StoredIn => "STORED_IN",
            Self::ScheduledIn => "SCHEDULED_IN",
            Self::Follows => "FOLLOWS",
            Self::RelatesTo => "RELATES_TO",
            Self::DerivedFrom => "DERIVED_FROM",
            Self::References => "REFERENCES",
        }
    }
}

/// A stable pointer to one node: its [`NodeLabel`] plus the external id that,
/// together with the workspace, is the idempotent `MERGE` key (SOUL §6.3 —
/// "MERGE on stable external ids"). The id is a uuid string for first-class
/// rows, or the uri for an external source.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NodeRef {
    /// The node's label.
    pub label: NodeLabel,
    /// The node's stable external id (uuid string, or uri).
    pub id: String,
}

impl NodeRef {
    /// Build a reference to a node of `label` with stable id `id`.
    #[must_use]
    pub fn new(label: NodeLabel, id: impl Into<String>) -> Self {
        Self {
            label,
            id: id.into(),
        }
    }

    /// A reference to a markdown [`Note`](catalerum_core::Note) node.
    #[must_use]
    pub fn note(id: NoteId) -> Self {
        Self::new(NodeLabel::Note, id.to_string())
    }

    /// A reference to a calendar [`Event`](catalerum_core::model::Event) node (§6.3/§8).
    #[must_use]
    pub fn event(id: EventId) -> Self {
        Self::new(NodeLabel::Event, id.to_string())
    }

    /// A reference to a [`Calendar`](catalerum_core::model::Calendar) node (§6.3/§8).
    #[must_use]
    pub fn calendar(id: CalendarId) -> Self {
        Self::new(NodeLabel::Calendar, id.to_string())
    }

    /// A reference to an [`Entity`](catalerum_core::Entity) node, labelled by
    /// its kind.
    #[must_use]
    pub fn entity(kind: EntityKind, id: EntityId) -> Self {
        Self::new(NodeLabel::from_entity_kind(kind), id.to_string())
    }

    /// The node a [`SourceRef`] projects to. Every first-class row maps to a
    /// labelled node in the §6.3 taxonomy; only [`SourceRef::External`] (an
    /// unmodelled uri) has no node and returns `None`.
    ///
    /// This backs link projection (SOUL §6.3): a `RELATES_TO` edge between any two
    /// objects. A thin node created here is enriched later by the object's own
    /// projection (`project_note`/`project_event`/…) under the same `(workspace,
    /// id)` MERGE key. The richer `:Email` projection — `SENT_BY`/`ADDRESSED_TO`
    /// edges to `:Person` nodes, §28 — is a separate future slice.
    #[must_use]
    pub fn from_source(source: &SourceRef) -> Option<Self> {
        match source {
            SourceRef::Event { id } => Some(Self::new(NodeLabel::Event, id.to_string())),
            SourceRef::Object { id } => Some(Self::new(NodeLabel::File, id.to_string())),
            SourceRef::Note { id } => Some(Self::new(NodeLabel::Note, id.to_string())),
            SourceRef::Memory { id } => Some(Self::new(NodeLabel::Memory, id.to_string())),
            SourceRef::Email { id } => Some(Self::new(NodeLabel::Email, id.to_string())),
            SourceRef::Message { id } => Some(Self::new(NodeLabel::Message, id.to_string())),
            SourceRef::Document { id } => Some(Self::new(NodeLabel::Document, id.to_string())),
            SourceRef::External { .. } => None,
        }
    }
}

/// A `(workspace_id, NodeRef)` pair — everything a writer needs to address a
/// node uniquely. Kept distinct from [`NodeRef`] so a caller can never forget
/// the workspace scope (SOUL §18: cross-workspace reach is impossible by
/// construction; every node is keyed on its workspace).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopedNode {
    pub workspace_id: WorkspaceId,
    pub node: NodeRef,
}

impl ScopedNode {
    #[must_use]
    pub fn new(workspace_id: WorkspaceId, node: NodeRef) -> Self {
        Self { workspace_id, node }
    }
}
