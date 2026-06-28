//! Strongly-typed entity identifiers.
//!
//! Every domain entity gets its own newtype wrapper around [`uuid::Uuid`] so
//! that an `EventId` can never be mistaken for a `NoteId` at a type level. The
//! [`id_type!`] macro derives the full ergonomic surface
//! (`new`/`from`/`into`/`Display`/`FromStr`/`Serialize`/`Deserialize`).
//!
//! All IDs default to UUIDv4 generation via [`Default`] / `new`.

// The `id_type!` macro is fully self-contained (all paths are absolute), so the
// module body itself needs no imports beyond the test module.

/// Defines a strongly-typed UUID newtype.
///
/// The generated type:
/// - wraps a [`Uuid`] (public field `.0` plus accessors),
/// - implements `Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug`,
/// - implements [`Serialize`]/[`Deserialize`] transparently as a UUID string,
/// - implements [`Display`](fmt::Display) and [`FromStr`],
/// - converts to/from [`Uuid`] via [`From`],
/// - generates a fresh v4 value via `new()` and [`Default`].
#[macro_export]
macro_rules! id_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Clone,
            Copy,
            PartialEq,
            Eq,
            Hash,
            PartialOrd,
            Ord,
            Debug,
            ::serde::Serialize,
            ::serde::Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub ::uuid::Uuid);

        impl $name {
            /// Generate a fresh random (v4) identifier.
            #[must_use]
            pub fn new() -> Self {
                Self(::uuid::Uuid::new_v4())
            }

            /// Wrap an existing [`Uuid`](::uuid::Uuid).
            #[must_use]
            pub const fn from_uuid(uuid: ::uuid::Uuid) -> Self {
                Self(uuid)
            }

            /// The inner [`Uuid`](::uuid::Uuid).
            #[must_use]
            pub const fn as_uuid(&self) -> ::uuid::Uuid {
                self.0
            }

            /// Consume into the inner [`Uuid`](::uuid::Uuid).
            #[must_use]
            pub const fn into_uuid(self) -> ::uuid::Uuid {
                self.0
            }

            /// The nil identifier (all zeroes). Useful as a sentinel.
            #[must_use]
            pub const fn nil() -> Self {
                Self(::uuid::Uuid::nil())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl ::core::fmt::Display for $name {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                ::core::fmt::Display::fmt(&self.0, f)
            }
        }

        impl ::core::str::FromStr for $name {
            type Err = ::uuid::Error;
            fn from_str(s: &str) -> ::core::result::Result<Self, Self::Err> {
                ::uuid::Uuid::from_str(s).map(Self)
            }
        }

        impl ::core::convert::From<::uuid::Uuid> for $name {
            fn from(uuid: ::uuid::Uuid) -> Self {
                Self(uuid)
            }
        }

        impl ::core::convert::From<$name> for ::uuid::Uuid {
            fn from(id: $name) -> ::uuid::Uuid {
                id.0
            }
        }
    };
}

id_type!(
    /// Identifies an [`Organisation`](crate::model::Organisation) — the
    /// administrative grouping above the tenancy boundary (SOUL §18). Org roles
    /// govern administration only and confer no data access; the workspace stays
    /// the sole data + capability boundary.
    OrganisationId
);
id_type!(
    /// Identifies a [`Workspace`](crate::model::Workspace) — the tenancy boundary.
    WorkspaceId
);
id_type!(
    /// Identifies a [`User`](crate::model::User).
    UserId
);
id_type!(
    /// Identifies an [`Agent`](crate::model::Agent).
    AgentId
);
id_type!(
    /// Identifies an [`AgentProfile`](crate::model::AgentProfile) — a persisted,
    /// channel-bindable, subagent-aware scoped agent configuration (SOUL §19).
    AgentProfileId
);
id_type!(
    /// Identifies a [`Connection`](crate::model::Connection) to an external provider.
    ConnectionId
);
id_type!(
    /// Identifies an [`McpServerDef`](crate::model::McpServerDef) — a persisted,
    /// runtime-managed external MCP server connection (SOUL §26).
    McpServerId
);
id_type!(
    /// Identifies a [`Calendar`](crate::model::Calendar).
    CalendarId
);
id_type!(
    /// Identifies an [`Event`](crate::model::Event).
    EventId
);
id_type!(
    /// Identifies a [`Bucket`](crate::model::Bucket).
    BucketId
);
id_type!(
    /// Identifies a [`StoredObject`](crate::model::StoredObject).
    ObjectId
);
id_type!(
    /// Identifies an [`ObjectLabel`](crate::model::ObjectLabel) — a user/agent
    /// applied label on a stored file or directory path (SOUL §9).
    ObjectLabelId
);
id_type!(
    /// Identifies a [`Mailbox`](crate::model::Mailbox).
    MailboxId
);
id_type!(
    /// Identifies an [`Email`](crate::model::Email).
    EmailId
);
id_type!(
    /// Identifies an [`Entity`](crate::model::Entity).
    EntityId
);
id_type!(
    /// Identifies a [`Document`](crate::model::Document).
    DocumentId
);
id_type!(
    /// Identifies a [`Chunk`](crate::model::Chunk).
    ChunkId
);
id_type!(
    /// Identifies a [`Note`](crate::model::Note).
    NoteId
);
id_type!(
    /// Identifies a [`Link`](crate::model::Link) — a user/agent-authored
    /// relationship between two objects (SOUL §5/§6.3).
    LinkId
);
id_type!(
    /// Identifies a [`Memory`](crate::model::Memory).
    MemoryId
);
id_type!(
    /// Identifies a [`Skill`](crate::model::Skill).
    SkillId
);
id_type!(
    /// Identifies a [`Board`](crate::model::Board).
    BoardId
);
id_type!(
    /// Identifies a [`Column`](crate::model::Column).
    ColumnId
);
id_type!(
    /// Identifies a [`Task`](crate::model::Task).
    TaskId
);
id_type!(
    /// Identifies a [`Channel`](crate::model::Channel).
    ChannelId
);
id_type!(
    /// Identifies a [`Grant`](crate::model::Grant).
    GrantId
);
id_type!(
    /// Identifies a [`Conversation`](crate::model::Conversation).
    ConversationId
);
id_type!(
    /// Identifies a [`Message`](crate::model::Message).
    MessageId
);
id_type!(
    /// Identifies an [`Automation`](crate::model::Automation).
    AutomationId
);
id_type!(
    /// Identifies an [`AutomationRun`](crate::model::AutomationRun) — one execution
    /// of an automation (SOUL §11).
    AutomationRunId
);
id_type!(
    /// Identifies an [`AutomationStep`](crate::model::AutomationStep) — one action
    /// within an [`AutomationRun`](crate::model::AutomationRun) (SOUL §11).
    AutomationStepId
);
id_type!(
    /// Identifies a [`UiDefinition`](crate::model::UiDefinition) — an AI-authored
    /// emerged UI (a declarative component tree the AI can create and edit).
    UiDefinitionId
);
id_type!(
    /// Identifies a [`TerminalSession`](crate::model::TerminalSession) — one
    /// interactive terminal an agent stood up (SOUL §20).
    TerminalSessionId
);
id_type!(
    /// Identifies a [`PendingQuestion`](crate::model::PendingQuestion) — an
    /// unanswered `ask_user` question form awaiting the user's reply, persisted so
    /// it survives a reload/reconnect (SOUL §7/§12).
    PendingQuestionId
);
id_type!(
    /// Identifies a [`PendingApproval`](crate::model::PendingApproval) — a guarded
    /// tool call deferred until the user approves/rejects it, persisted so it
    /// survives a reload/reconnect/restart (SOUL §7/§12/§19).
    PendingApprovalId
);
id_type!(
    /// Identifies an [`McpEndpoint`](crate::model::McpEndpoint) — a user-authored,
    /// Boa-scripted MCP endpoint exposing scoped tools (e.g. prefix-scoped semantic
    /// search over one bucket subdir) to external agents (SOUL §26).
    McpEndpointId
);
id_type!(
    /// Identifies a **computer agent** — an installed daemon on a server/desktop
    /// that a workspace enrolls so the LLM can drive files, search, commands, and
    /// (opt-in) desktop control on that machine over an authenticated outbound
    /// WebSocket (SOUL §19/§20). Its long-lived enrollment token is stored hashed;
    /// the row is workspace- and owner-scoped.
    ComputerAgentId
);

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn roundtrip_display_fromstr() {
        let id = EventId::new();
        let s = id.to_string();
        let parsed: EventId = s.parse().unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn serde_is_transparent_string() {
        let id = WorkspaceId::from_uuid(Uuid::nil());
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"00000000-0000-0000-0000-000000000000\"");
        let back: WorkspaceId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn uuid_conversions() {
        let u = Uuid::new_v4();
        let id: NoteId = u.into();
        let back: Uuid = id.into();
        assert_eq!(u, back);
    }
}
