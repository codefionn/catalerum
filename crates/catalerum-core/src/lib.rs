//! catalerum-core — the canonical, provider-agnostic domain model, strongly-typed
//! IDs, the capability model, error types, and the shared provider/tool traits.
//!
//! This crate is the **dependency root**: everything in the workspace depends on
//! it, and it depends on nothing in the workspace (SOUL §4). It names **no**
//! concrete provider — calendars, buckets, the LLM, executors, and channels are
//! reached only through the traits in [`provider`] (SOUL §3.2).
//!
//! # Module map
//! - [`id`] — strongly-typed UUID newtypes for every entity + the [`id_type!`]
//!   macro.
//! - [`error`] — the crate-wide [`Error`] / [`Result`].
//! - [`model`] — the §5 domain structs/enums (the catalogue).
//! - [`capability`] — the §19 capability/grant model + matcher/attenuation.
//! - [`llm`] — provider-agnostic chat request/response shapes (§7).
//! - [`embed`] — provider-agnostic embeddings request/response shapes (§6.4/§7).
//! - [`audio`] — provider-agnostic TTS/STT request/response shapes (§7).
//! - [`stream`] — the [`StreamEvent`](stream::StreamEvent) streaming enum (§7).
//! - [`tool`] — the [`Tool`](tool::Tool) trait + [`ToolRegistry`](tool::ToolRegistry) (§3.3, §7).
//! - [`provider`] — the shared provider traits (§7/§8/§9/§20/§25).
//!
//! Most common types are re-exported at the crate root for ergonomic `use
//! catalerum_core::…;`.

#![forbid(unsafe_code)]

pub mod ask;
pub mod audio;
pub mod capability;
pub mod computer;
pub mod embed;
pub mod error;
pub mod id;
pub mod llm;
pub mod model;
pub mod model_ui;
pub mod ocr;
pub mod preview;
pub mod provider;
pub mod stream;
pub mod tool;

// --- Root re-exports: the stable, ergonomic surface. ----------------------

pub use error::{Error, Result};

pub use id::{
    AgentId, AgentProfileId, AutomationId, AutomationRunId, AutomationStepId, BoardId, BucketId,
    CalendarId, ChannelId, ChunkId, ColumnId, ComputerAgentId, ConnectionId, ConversationId,
    DocumentId, EmailId, EntityId, EventId, GrantId, LinkId, MailboxId, McpEndpointId, McpServerId,
    MemoryId, MessageId, NoteId, ObjectId, ObjectLabelId, OrganisationId, PendingApprovalId,
    SkillId, TaskId, TerminalSessionId, UserId, WorkspaceId,
};

pub use computer::{
    AgentToServer, ComputerCapabilities, ComputerOp, ComputerPlatform, DesktopAction, DirGrant,
    DirMode, ExecPolicy, OpResponse, SandboxKind, ServerToAgent, WriteMode, PROTOCOL_VERSION,
};

pub use model::{
    Agent, AgentProfile, Attachment, Author, Automation, AutomationRun, AutomationStep, Board,
    Bucket, Calendar, Channel as ChannelModel, ChannelKind, Chunk, Code, Column, Connection,
    ConnectionKind, Conversation, CreationPolicy, Cursor, Document, Email, EmailAddress, Entity,
    EntityKind, EntityRef, Event, ExecutorKind, ExtractedAttachment, Grant, Link, Mailbox, Map,
    McpAuthSpec, McpServerDef, Membership, Memory, MemoryScope, Message, MessageRole, Note,
    ObjectLabel, OrgMembership, OrgRole, Organisation, Origin, Profile, Role, RunStatus, Skill,
    SkillInvocation, SourceRef, StepStatus, StoredObject, Subject, Task, TaskStatus,
    TerminalSession, TerminalSessionStatus, ToolCall, UiDefinition, User, Workspace,
};

pub use id::UiDefinitionId;

pub use model_ui::{
    apply_ui_patch, get_path, set_path, stringify, truthy, validate_ui_spec, ClientOp, ComputedDef,
    EventName, ForEach, Handler, NodeKind, ScriptDef, UiAction, UiNode, UiPatchOp, UiSpec,
    UiSpecError, UiView, ValidationKind, ValidationRule,
};

pub use capability::{
    allows, attenuate, Action, AttenuationError, Capability, Constraints, Resource, TimeWindow,
};

pub use llm::{ChatMessage, ChatRequest, MediaInput, ToolChoice, ToolSpec};

pub use embed::{Embedding, EmbeddingRequest, EmbeddingResponse};

pub use audio::{SpeechAudio, SpeechRequest, TranscriptionRequest, TranscriptionResponse};

pub use ocr::{OcrRequest, OcrResponse};

pub use preview::{PreviewFormat, PreviewRequest, PreviewResponse};

pub use stream::{FinishReason, ReasoningDetail, StreamEvent, Usage};

pub use tool::{Tool, ToolContext, ToolRegistry};

pub use ask::{Answer, Question};

pub use id::PendingQuestionId;
pub use model::PendingQuestion;

pub use provider::{
    ByteStream, CalendarProvider, Channel, ChannelTarget, CommandResult, CommandSpec,
    EmailProvider, Embedder, Executor, FetchFormat, FetchMode, FetchRequest, FetchedPage,
    InMessage, LlmClient, NewEvent, ObjectMeta, OcrEngine, OutMessage, Previewer, PutMeta,
    ResourceLimits, SearchHit, SearchRequest, SearchResults, Session, SessionSpec,
    SpeechSynthesizer, StorageBackend, SyncBatch, Transcriber, WebFetcher, WebSearcher,
};
