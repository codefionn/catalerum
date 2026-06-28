//! Shared provider traits (SOUL §3.2): the core knows no concrete provider.
//!
//! Every external integration — LLM, calendars, storage, executors, channels —
//! is reached only through one of these traits. Concrete impls live in the
//! provider crates (`catalerum-llm`, `catalerum-calendar`, `catalerum-storage`,
//! `catalerum-exec`, `catalerum-channels`). If core code names a vendor outside
//! a provider crate, it's a bug.
//!
//! Async methods use [`async_trait`]. Streaming methods return
//! [`futures::stream::BoxStream`] so impls stay object-safe and pluggable.

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::audio::{SpeechAudio, SpeechRequest, TranscriptionRequest, TranscriptionResponse};
use crate::embed::{EmbeddingRequest, EmbeddingResponse};
use crate::error::{Error, Result};
use crate::id::WorkspaceId;
use crate::llm::ChatRequest;
use crate::model::{Calendar, Cursor, Email, Event, Mailbox};
use crate::ocr::{OcrRequest, OcrResponse};
use crate::preview::{PreviewRequest, PreviewResponse};
use crate::stream::StreamEvent;

// ---------------------------------------------------------------------------
// LLM (SOUL §7)
// ---------------------------------------------------------------------------

/// An async, streaming chat client (SOUL §7). The concrete impl
/// (`catalerum-llm`) targets llmleaf/OpenRouter; core only sees the trait.
#[async_trait]
pub trait LlmClient: Send + Sync {
    /// Run a streaming chat completion. The returned stream yields
    /// [`StreamEvent`]s and is guaranteed to end with a
    /// [`StreamEvent::Done`](crate::stream::StreamEvent::Done) on success.
    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent>>>;
}

/// Generates embedding vectors via llmleaf (SOUL §6.4/§7). llmleaf is
/// multi-modal, so the concrete impl is the same `catalerum-llm` client used for
/// chat. The vectors feed the derived Qdrant index (`catalerum-vector`).
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Embed the request's inputs, returning one vector per input in input order.
    async fn embed(&self, request: EmbeddingRequest) -> Result<EmbeddingResponse>;
}

/// Synthesizes speech — text-to-speech — via llmleaf (SOUL §7). Concrete impl:
/// the `catalerum-llm` client.
#[async_trait]
pub trait SpeechSynthesizer: Send + Sync {
    /// Render `request.input` to audio bytes in the requested format.
    async fn synthesize(&self, request: SpeechRequest) -> Result<SpeechAudio>;
}

/// Transcribes audio — speech-to-text — via llmleaf (SOUL §7). Concrete impl:
/// the `catalerum-llm` client.
#[async_trait]
pub trait Transcriber: Send + Sync {
    /// Transcribe the request's audio to text.
    async fn transcribe(&self, request: TranscriptionRequest) -> Result<TranscriptionResponse>;
}

/// Extracts the text of an image/PDF document — OCR (SOUL §7/§10). Concrete
/// impls: a Mistral-style `/v1/ocr` API client and the offline `tesseract`
/// fallback (`catalerum-ocr`), a vision chat model via llmleaf
/// (`catalerum-llm`), and a fallback chain composing them.
#[async_trait]
pub trait OcrEngine: Send + Sync {
    /// A short engine id (`mistral`, `vision`, `tesseract`, …) for logs,
    /// status, and [`OcrResponse::engine`].
    fn name(&self) -> &'static str;

    /// Whether this engine can OCR a document of `content_type` (parameters
    /// like `; charset=…` are stripped before matching). Unknown types are
    /// refused, mirroring how text extraction never guesses.
    fn supports(&self, content_type: &str) -> bool;

    /// Extract the request's document text.
    async fn ocr(&self, request: OcrRequest) -> Result<OcrResponse>;
}

/// Renders a document to a raster **image** preview (SOUL §9/§10): the first
/// page of a PDF/office document, a rendered spreadsheet/presentation, or a
/// resized thumbnail of an image. Concrete impls: a pure-Rust `image`-crate
/// engine for image formats and a sandbox-backed engine that shells the
/// LibreOffice/poppler/pymupdf toolchain (`catalerum-preview`), composed by a
/// fallback chain — mirroring [`OcrEngine`].
#[async_trait]
pub trait Previewer: Send + Sync {
    /// A short engine id (`image`, `sandbox`, …) for logs, status, and
    /// [`PreviewResponse::engine`].
    fn name(&self) -> &'static str;

    /// Whether this engine can preview a document of `content_type` (parameters
    /// like `; charset=…` are stripped before matching). Unknown types are
    /// refused, so the chain routes elsewhere.
    fn supports(&self, content_type: &str) -> bool;

    /// Render the request's document to an image.
    async fn preview(&self, request: PreviewRequest) -> Result<PreviewResponse>;
}

// ---------------------------------------------------------------------------
// Calendar (SOUL §8)
// ---------------------------------------------------------------------------

/// A batch of incrementally-synced items plus the next cursor (SOUL §8). Sync is
/// idempotent and incremental (SOUL §3.4): re-running from `next_cursor` never
/// duplicates and never loses unsynced edits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncBatch<T> {
    /// Items created or updated since the request cursor.
    pub upserts: Vec<T>,
    /// Stable external ids of items deleted since the request cursor.
    #[serde(default)]
    pub deletions: Vec<String>,
    /// Cursor to pass on the next sync call.
    pub next_cursor: Cursor,
    /// True if more data is immediately available (paged sync).
    #[serde(default)]
    pub has_more: bool,
}

/// A not-yet-persisted event to create on a provider (SOUL §8).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewEvent {
    pub summary: String,
    pub start: chrono::DateTime<chrono::Utc>,
    pub end: chrono::DateTime<chrono::Utc>,
    /// A whole-day event: the stamps mark calendar *dates* (midnight UTC) and
    /// providers write date-valued endpoints (`VALUE=DATE` / `{date}` /
    /// `isAllDay`) instead of instants.
    #[serde(default)]
    pub all_day: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rrule: Option<String>,
    /// Attendee email/identifiers, in provider-native form.
    #[serde(default)]
    pub attendees: Vec<String>,
    /// Category labels (iCalendar `CATEGORIES`).
    #[serde(default)]
    pub labels: Vec<String>,
    /// File / image attachments (iCalendar `ATTACH`).
    #[serde(default)]
    pub attachments: Vec<crate::model::Attachment>,
}

/// A calendar provider (SOUL §8). Impls: local ics, CalDAV/webcal, Google.
#[async_trait]
pub trait CalendarProvider: Send + Sync {
    /// Enumerate the calendars this provider exposes.
    async fn list_calendars(&self) -> Result<Vec<Calendar>>;

    /// Incrementally sync events for `cal` from `cursor` (sync-token / ETag).
    async fn sync(&self, cal: &Calendar, cursor: Option<Cursor>) -> Result<SyncBatch<Event>>;

    /// Whether [`sync`](Self::sync) returns incremental **deltas** rather than a
    /// full snapshot of the calendar — the mirror of
    /// [`EmailProvider::is_incremental`]. `false` (the default, e.g. the local
    /// `.ics` backend whose cursor is a whole-file content hash) means `upserts` is
    /// a full snapshot, so a collect trigger must de-dup against already-collected
    /// uids; `true` (CalDAV `sync-collection`, Google `syncToken`) means the
    /// provider is authoritative for deltas/deletions and the consumer must not
    /// diff-reconcile.
    fn is_incremental(&self) -> bool {
        false
    }

    /// Create an event on the provider.
    async fn create_event(&self, cal: &Calendar, event: NewEvent) -> Result<Event>;

    /// Update an existing event (honouring ETag/sequence).
    async fn update_event(&self, event: &Event) -> Result<Event>;

    /// Delete an event.
    async fn delete_event(&self, event: &Event) -> Result<()>;
}

// ---------------------------------------------------------------------------
// Email (read-only ingest, SOUL §28)
// ---------------------------------------------------------------------------

/// An email provider (SOUL §28) — a **read-only** ingest source, the same shape
/// as a [`CalendarProvider`]: pull messages on a cursor, normalize to the
/// canonical [`Email`]. catalerum reads mail; it never sends/replies (§14), so
/// there is no write half. Impls: local Maildir, IMAP, JMAP, Gmail (M7).
#[async_trait]
pub trait EmailProvider: Send + Sync {
    /// Enumerate the mailboxes (folders) this provider exposes.
    async fn list_mailboxes(&self) -> Result<Vec<Mailbox>>;

    /// Incrementally sync messages for `mailbox` from `cursor` (IMAP
    /// `UIDVALIDITY/UIDNEXT`, JMAP state, Maildir scan position). Idempotent:
    /// re-running from `next_cursor` upserts by `(mailbox_id, uid)` and never
    /// duplicates (SOUL §3.4).
    async fn sync(&self, mailbox: &Mailbox, cursor: Option<Cursor>) -> Result<SyncBatch<Email>>;

    /// Whether [`sync`](Self::sync) returns incremental **deltas** — only the
    /// messages that changed since `cursor` in [`upserts`](SyncBatch::upserts),
    /// with every removal named in [`deletions`](SyncBatch::deletions) — rather
    /// than a full snapshot of the mailbox.
    ///
    /// This governs how the ingest worker reconciles deletions (SOUL §3.4):
    /// - `false` (the default, e.g. local **Maildir**): `upserts` is a full
    ///   snapshot, so the worker may delete any stored uid absent from it.
    /// - `true` (IMAP/JMAP/Gmail): the provider is **authoritative** for
    ///   deletions and the worker must NOT diff-reconcile — otherwise a small
    ///   delta of new mail (with no deletions) would look like "everything else
    ///   was removed" and wipe the mailbox.
    fn is_incremental(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// Storage (SOUL §9)
// ---------------------------------------------------------------------------

/// Provider-native metadata for an object in a bucket (SOUL §9).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectMeta {
    pub key: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    pub last_modified: chrono::DateTime<chrono::Utc>,
}

/// Metadata supplied when writing an object (SOUL §9).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PutMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_length: Option<u64>,
}

/// A stream of object bytes (chunked), used by `get`/`put` (SOUL §9).
pub type ByteStream = BoxStream<'static, Result<Vec<u8>>>;

/// A storage backend (SOUL §9). Impls: local FS, S3, WebDAV. Blobs stay in the
/// bucket; the DB stores only catalogued metadata (SOUL §14).
#[async_trait]
pub trait StorageBackend: Send + Sync {
    /// List objects under `prefix`.
    async fn list(&self, prefix: &str) -> Result<BoxStream<'static, Result<ObjectMeta>>>;

    /// Stat a single object.
    async fn stat(&self, key: &str) -> Result<ObjectMeta>;

    /// Stream an object's bytes.
    async fn get(&self, key: &str) -> Result<ByteStream>;

    /// Write an object from a byte stream.
    async fn put(&self, key: &str, data: ByteStream, meta: PutMeta) -> Result<()>;

    /// Delete an object.
    async fn delete(&self, key: &str) -> Result<()>;

    /// Idempotently provision the backend's container (an S3 bucket, a WebDAV
    /// collection) if it does not already exist. Default: a **no-op** — the local
    /// filesystem backend creates directories lazily on `put`. A backend that needs
    /// the container to pre-exist overrides this; the binary calls it once at
    /// startup so a fresh deployment self-heals.
    async fn ensure_container(&self) -> Result<()> {
        Ok(())
    }
}

/// The **physical**, workspace-namespaced backend key for a user-facing object
/// `key` in `workspace_id` (SOUL §18). The blob backend is a single shared store
/// across all workspaces, so the catalogue's per-workspace scoping must be
/// mirrored at the byte layer: every physical key is prefixed with the workspace
/// id, making cross-tenant blob access (list / read / overwrite / delete)
/// impossible by construction. The catalogue (`objects.key`) and the API surface
/// keep the **user-facing** key; only the bytes live under the namespaced key.
/// Both the storage routes (writes/reads) and the ingest worker (reads) MUST use
/// this one convention so they agree on where a blob lives.
#[must_use]
pub fn workspace_object_key(workspace_id: WorkspaceId, key: &str) -> String {
    format!("{workspace_id}/{}", key.trim_start_matches('/'))
}

/// Recover the user-facing key from a [`workspace_object_key`] physical key,
/// stripping the `<workspace_id>/` namespace. Returns the input unchanged if it
/// lacks the expected prefix (defensive — a backend should only ever return keys
/// under the queried prefix).
#[must_use]
pub fn strip_workspace_key(workspace_id: WorkspaceId, physical_key: &str) -> String {
    physical_key
        .strip_prefix(&format!("{workspace_id}/"))
        .unwrap_or(physical_key)
        .to_string()
}

// ---------------------------------------------------------------------------
// Executor (SOUL §20)
// ---------------------------------------------------------------------------

/// A command/code to run via an [`Executor`] (SOUL §20). Either an `argv`
/// invocation or inline `code` for a language; both gated by `exec:*`
/// capabilities and an allow-list.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSpec {
    /// Program + arguments to execute (mutually exclusive with `code`).
    #[serde(default)]
    pub argv: Vec<String>,
    /// Inline code to run in `language` (mutually exclusive with `argv`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Language for `code` (e.g. `python`), matching `exec:run@bao{lang=…}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Environment variables to set.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Working directory inside the sandbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Data piped to stdin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdin: Option<String>,
    /// Wall-clock timeout in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// Resource limits (cpu/mem/net), provider-interpreted.
    #[serde(default, skip_serializing_if = "ResourceLimits::is_empty")]
    pub limits: ResourceLimits,
}

/// Sandbox resource limits (SOUL §20).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceLimits {
    /// CPU cores (whole units).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<u32>,
    /// Memory limit in megabytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u64>,
    /// Network policy (e.g. `none`, `egress`); provider-interpreted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,
}

impl ResourceLimits {
    fn is_empty(&self) -> bool {
        self.cpu.is_none() && self.memory_mb.is_none() && self.network.is_none()
    }
}

/// The result of running a [`CommandSpec`] (SOUL §20).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    /// True if the command was killed by the timeout.
    #[serde(default)]
    pub timed_out: bool,
}

/// An interactive session handle (SOUL §20). Concrete shape is owned by
/// `catalerum-exec`; core fixes the opaque identifier plus the on-disk working
/// directory the session runs in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    /// Backend-assigned session id.
    pub id: String,
    /// Host working directory the session runs in, when the backend exposes one
    /// (local/sandbox; `None` for container/k8s where files live in the
    /// container/pod). Lets an ephemeral session be flushed to object storage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_dir: Option<String>,
    /// Working directory **inside** the backend (a container/pod path for
    /// sandboxed sessions). Set when `host_dir` is `None` so file ops that must
    /// land in the session's workdir (`stage_object`) can still target it via
    /// the backend's copy channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// How to open an interactive [`Session`] (SOUL §20). The backend is chosen by
/// which [`Executor`] is invoked; this carries the per-session shape.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSpec {
    /// Working directory to start in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Environment to set for the session shell.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Initial terminal width in columns (0 → backend default, e.g. 80).
    #[serde(default)]
    pub cols: u16,
    /// Initial terminal height in rows (0 → backend default, e.g. 24).
    #[serde(default)]
    pub rows: u16,
    /// Resource limits (cpu/mem/net), provider-interpreted.
    #[serde(default, skip_serializing_if = "ResourceLimits::is_empty")]
    pub limits: ResourceLimits,
    /// Container image (container/k8s backends; `None` → the backend default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Shell/program to launch (`None` → the backend default, e.g. `$SHELL`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
}

/// A pluggable command/code executor (SOUL §20). Backends: Local, Sandbox,
/// Container (docker/podman, default sandbox), Kubernetes, bao. Selected per
/// config / agent grant; Local is protected and opt-in.
///
/// One-shot [`run`](Executor::run) and [`open_session`](Executor::open_session)
/// are required; the remaining interactive-session methods default to
/// [`Error::Unsupported`] so a backend opts in as it grows a real PTY.
#[async_trait]
pub trait Executor: Send + Sync {
    /// Run a one-shot command/code spec.
    async fn run(&self, cmd: CommandSpec) -> Result<CommandResult>;

    /// Open a longer-lived interactive (PTY-backed) session.
    async fn open_session(&self, spec: SessionSpec) -> Result<Session>;

    /// Write bytes (keystrokes / a command line) to a session's stdin.
    async fn session_write(&self, session: &Session, data: Vec<u8>) -> Result<()> {
        let _ = (session, data);
        Err(Error::Unsupported(
            "this executor has no interactive sessions".into(),
        ))
    }

    /// Drain up to `max_bytes` (0 = all) of output buffered since the last read.
    async fn session_read(&self, session: &Session, max_bytes: usize) -> Result<Vec<u8>> {
        let _ = (session, max_bytes);
        Err(Error::Unsupported(
            "this executor has no interactive sessions".into(),
        ))
    }

    /// Subscribe to a session's live output byte stream (for a read-only pane).
    async fn session_output(&self, session: &Session) -> Result<ByteStream> {
        let _ = session;
        Err(Error::Unsupported(
            "this executor has no interactive sessions".into(),
        ))
    }

    /// Resize a session's PTY.
    async fn session_resize(&self, session: &Session, cols: u16, rows: u16) -> Result<()> {
        let _ = (session, cols, rows);
        Err(Error::Unsupported(
            "this executor has no interactive sessions".into(),
        ))
    }

    /// Close a session, terminating its process / PTY.
    async fn close_session(&self, session: &Session) -> Result<()> {
        let _ = session;
        Err(Error::Unsupported(
            "this executor has no interactive sessions".into(),
        ))
    }

    /// Reap sessions whose process exited on its own — the user ran `exit`, the
    /// shell crashed — without an explicit [`close_session`](Self::close_session),
    /// tearing down each one's PTY plus any external container / Pod kept for it.
    /// Returns the reaped session ids so the caller can close their durable rows.
    /// Default: this executor keeps no sessions, so there is nothing to reap.
    async fn reap(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
}

// ---------------------------------------------------------------------------
// Channels (SOUL §25)
// ---------------------------------------------------------------------------

/// Where to deliver an outbound message (SOUL §25). Provider-native target
/// (room id / chat id / channel id).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChannelTarget(pub String);

/// An outbound message to a channel (SOUL §25).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OutMessage {
    /// Markdown/plain text body.
    pub body: String,
    /// Optional structured payload (attachments, formatting), provider-interpreted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<Json>,
}

/// An inbound message from a channel (SOUL §25). Surfaces as a `ChannelMessage`
/// automation trigger and can open a `Conversation { origin: Channel }`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InMessage {
    /// Where the message came from (room/chat id).
    pub source: ChannelTarget,
    /// Provider-native sender identifier.
    pub sender: String,
    /// Message body.
    pub body: String,
    /// Provider-native message id (for threading / dedup).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
}

/// A bidirectional messaging channel (SOUL §25). Impls: Matrix, Telegram,
/// Discord.
#[async_trait]
pub trait Channel: Send + Sync {
    /// Deliver an outbound message.
    async fn send(&self, target: ChannelTarget, msg: OutMessage) -> Result<()>;

    /// Subscribe to inbound messages.
    async fn subscribe(&self) -> Result<BoxStream<'static, Result<InMessage>>>;
}

// ---------------------------------------------------------------------------
// Web fetching & browsing (SOUL §27)
// ---------------------------------------------------------------------------

/// The representation a [`WebFetcher`] returns for a page (SOUL §27).
///
/// `Markdown` is the default because it is what the LLM should usually see:
/// clean, structural, and far cheaper in context than raw HTML (SOUL §27).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FetchFormat {
    /// Cleaned, AI-friendly Markdown — the default; minimises context use.
    #[default]
    Markdown,
    /// The raw HTML exactly as fetched (after JS rendering in browser mode).
    Html,
    /// Plain text: the Markdown with link/emphasis/heading syntax stripped.
    Text,
}

/// How a page is retrieved (SOUL §27). The fetcher maps this onto a concrete
/// backend: `Http` is the local-first plain GET; `Browser` drives a real
/// JS-capable browser (Chrome DevTools Protocol / Playwright / Firecrawl);
/// `Auto` lets the fetcher pick (plain GET first, browser on demand).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FetchMode {
    /// Backend's choice (plain HTTP unless it decides rendering is needed).
    #[default]
    Auto,
    /// Plain HTTP GET — no JavaScript, lowest cost, local-first.
    Http,
    /// A controlled browser that executes JavaScript before snapshotting.
    Browser,
}

/// A request to fetch a single web resource (SOUL §27).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchRequest {
    /// Absolute `http(s)` URL to fetch.
    pub url: String,
    /// Representation to return (default [`FetchFormat::Markdown`]).
    #[serde(default)]
    pub format: FetchFormat,
    /// Retrieval strategy (default [`FetchMode::Auto`]).
    #[serde(default)]
    pub mode: FetchMode,
    /// Extract only the main article content, dropping nav/header/footer/aside
    /// boilerplate before conversion (SOUL §27). On by default.
    #[serde(default = "default_true")]
    pub main_content_only: bool,
    /// Browser mode: CSS selector to wait for before snapshotting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_for: Option<String>,
    /// Per-request timeout override, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

fn default_true() -> bool {
    true
}

impl FetchRequest {
    /// A default Markdown, auto-mode fetch of `url`.
    #[must_use]
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            format: FetchFormat::Markdown,
            mode: FetchMode::Auto,
            main_content_only: true,
            wait_for: None,
            timeout_secs: None,
        }
    }

    /// Builder: set the return [`FetchFormat`].
    #[must_use]
    pub fn format(mut self, format: FetchFormat) -> Self {
        self.format = format;
        self
    }

    /// Builder: set the [`FetchMode`].
    #[must_use]
    pub fn mode(mut self, mode: FetchMode) -> Self {
        self.mode = mode;
        self
    }
}

/// A fetched and normalised web page (SOUL §27). Carries both the requested
/// representation and the byte sizes, so callers can report how much context the
/// HTML→Markdown conversion saved.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchedPage {
    /// The final URL after following redirects.
    pub url: String,
    /// HTTP status code of the final response.
    pub status: u16,
    /// `<title>` of the page, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// `Content-Type` reported by the server, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// The page in the requested [`format`](FetchedPage::format).
    pub content: String,
    /// Which representation `content` holds.
    pub format: FetchFormat,
    /// Size in bytes of the original fetched HTML (pre-conversion).
    pub raw_bytes: u64,
    /// Size in bytes of the returned `content`.
    pub content_bytes: u64,
}

impl FetchedPage {
    /// Fraction of the original HTML's bytes the returned `content` occupies
    /// (`content_bytes / raw_bytes`), clamped to `[0, 1]`. A small number means
    /// the conversion saved a lot of context (SOUL §27). Returns `None` when the
    /// original size is unknown (e.g. a backend that returns Markdown directly).
    #[must_use]
    pub fn context_ratio(&self) -> Option<f64> {
        if self.raw_bytes == 0 {
            return None;
        }
        Some((self.content_bytes as f64 / self.raw_bytes as f64).clamp(0.0, 1.0))
    }
}

/// Fetches and normalises web resources for the LLM (SOUL §27).
///
/// Impls live in `catalerum-fetch`: a plain-HTTP backend (local-first), a
/// headless-browser backend over the Chrome DevTools Protocol (the Playwright /
/// Chromium "control" path), and a Firecrawl backend (self-hosted or cloud).
/// Every backend returns AI-friendly Markdown by default to minimise the context
/// a fetched page costs the model.
#[async_trait]
pub trait WebFetcher: Send + Sync {
    /// Fetch one resource and return it in the requested representation.
    async fn fetch(&self, request: FetchRequest) -> Result<FetchedPage>;
}

// ---------------------------------------------------------------------------
// Webhook delivery (SOUL §11/§27)
// ---------------------------------------------------------------------------

/// HTTP method of an outbound [`WebhookDelivery`]. Deliveries are writes by
/// nature, so only the write verbs are offered — a GET "delivery" is a fetch
/// ([`WebFetcher`]) and gates on `web:read` instead.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebhookMethod {
    /// The webhook default.
    #[default]
    Post,
    Put,
    Patch,
}

impl WebhookMethod {
    /// Parse a loose method token (`post` / `put` / `patch`, any case).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "post" => Some(Self::Post),
            "put" => Some(Self::Put),
            "patch" => Some(Self::Patch),
            _ => None,
        }
    }

    /// The canonical uppercase method string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
        }
    }
}

/// The body of an outbound [`WebhookDelivery`]: a JSON value (sent as
/// `application/json`, the webhook norm) or a raw string with an explicit
/// content type (form-encoded, plain text, XML, …).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebhookBody {
    /// A JSON payload, serialized and sent as `application/json`.
    Json(serde_json::Value),
    /// A raw string body with an explicit `Content-Type`.
    Raw { body: String, content_type: String },
}

/// One outbound webhook delivery (SOUL §11/§27): push a payload to an external
/// `http(s)` URL. The egress-**write** counterpart to a [`FetchRequest`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WebhookDelivery {
    /// Absolute `http(s)` URL to deliver to.
    pub url: String,
    /// HTTP method (default [`WebhookMethod::Post`]).
    #[serde(default)]
    pub method: WebhookMethod,
    /// Extra request headers (e.g. an `Authorization` bearer or an
    /// `X-Signature`). Hop-by-hop and body-framing headers are refused by the
    /// sender; the body's `Content-Type` comes from [`WebhookBody`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<(String, String)>,
    /// The payload.
    pub body: WebhookBody,
    /// Per-delivery timeout override, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

impl WebhookDelivery {
    /// A default POST of a JSON `payload` to `url`.
    #[must_use]
    pub fn json(url: impl Into<String>, payload: serde_json::Value) -> Self {
        Self {
            url: url.into(),
            method: WebhookMethod::Post,
            headers: Vec::new(),
            body: WebhookBody::Json(payload),
            timeout_secs: None,
        }
    }
}

/// The receiver's response to a completed [`WebhookDelivery`] — any HTTP
/// exchange that ran to completion, **including** non-2xx statuses (the caller
/// decides whether a 4xx/5xx is an error; the delivery tool treats it as one).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebhookResponse {
    /// The delivered-to URL.
    pub url: String,
    /// HTTP status code returned by the receiver.
    pub status: u16,
    /// `Content-Type` reported by the receiver, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// The response body (capped by the sender — an ack/error payload, not page
    /// content).
    pub body: String,
    /// Total bytes the receiver sent (may exceed `body.len()` when capped).
    pub body_bytes: u64,
}

impl WebhookResponse {
    /// Whether the receiver acknowledged with a 2xx.
    #[must_use]
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// Delivers outbound webhooks (SOUL §11/§27).
///
/// The impl lives in `catalerum-fetch` (`HttpWebhookSender`): a `reqwest`
/// client behind the same SSRF guard as web fetching — URL validation, DNS
/// re-resolution, and connect-time address screening; redirects are **not**
/// followed (a redirect is returned as its 3xx status, so the guard can't be
/// bounced around and a delivery never lands anywhere but the named URL).
#[async_trait]
pub trait WebhookSender: Send + Sync {
    /// Run one delivery to completion and return the receiver's response.
    /// `Err` is reserved for transport/validation failures (blocked URL, bad
    /// header, timeout, connection refused) — an HTTP error status is an
    /// `Ok(WebhookResponse)` with that status.
    async fn deliver(&self, delivery: WebhookDelivery) -> Result<WebhookResponse>;
}

// ---------------------------------------------------------------------------
// Web search (SOUL §27)
// ---------------------------------------------------------------------------

/// One ranked result from a web search (SOUL §27). The fields are the common
/// denominator across engines; an engine that lacks one leaves it `None` rather
/// than inventing it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    /// Result title / headline.
    pub title: String,
    /// Absolute result URL.
    pub url: String,
    /// Engine-provided description / excerpt, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    /// Full page text the engine returned when `include_raw_content` was asked
    /// for (Tavily/Exa). Costs more context, so it is opt-in (SOUL §27).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_content: Option<String>,
    /// Relevance score the engine assigned, if any (engine-relative, not
    /// comparable across providers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    /// Publish / last-seen date the engine reported, if any (free-form, usually
    /// ISO-8601 or a relative age like `2 days ago`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published: Option<String>,
}

/// A web-search request (SOUL §27). Mirrors [`FetchRequest`]'s shape: a required
/// query plus optional knobs with sensible serde defaults.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchRequest {
    /// The search query.
    pub query: String,
    /// Maximum number of results to return (default [`default_search_limit`]).
    #[serde(default = "default_search_limit")]
    pub limit: u32,
    /// Provider override (`brave`/`tavily`/…). `None` resolves to the configured
    /// default backend — the same "auto" idea as [`FetchMode::Auto`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Ask the engine for the full page text of each hit (slower / pricier, and
    /// not every engine supports it). Off by default to keep results cheap.
    #[serde(default)]
    pub include_raw_content: bool,
    /// Recency filter the engine understands, if any (e.g. `day`/`week`/`month`).
    /// Passed through best-effort; engines that don't support it ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freshness: Option<String>,
}

fn default_search_limit() -> u32 {
    5
}

impl SearchRequest {
    /// A default search of `query` (5 results, default backend, no raw content).
    #[must_use]
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            limit: default_search_limit(),
            provider: None,
            include_raw_content: false,
            freshness: None,
        }
    }

    /// Builder: cap the number of results.
    #[must_use]
    pub fn limit(mut self, limit: u32) -> Self {
        self.limit = limit;
        self
    }

    /// Builder: pin a specific provider.
    #[must_use]
    pub fn provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = Some(provider.into());
        self
    }
}

/// A completed web search (SOUL §27): the ranked hits plus a little provenance.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SearchResults {
    /// The query that produced these results.
    pub query: String,
    /// Which backend actually served the request (e.g. `brave`).
    pub provider: String,
    /// Ranked results, best first.
    pub results: Vec<SearchHit>,
    /// The engine's synthesized answer, when it returns one (Tavily).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
}

/// Searches the web for the LLM (SOUL §27).
///
/// Impls live in `catalerum-search` — one per provider (Brave, Tavily, Exa,
/// SearXNG, Google Programmable Search, SerpAPI) — plus a `MultiSearcher` that
/// routes a request to a backend by [`SearchRequest::provider`] (or the
/// configured default). Unlike [`WebFetcher`], the trait carries [`name`] so the
/// router can dispatch by provider id and the API can enumerate what is wired.
///
/// [`name`]: WebSearcher::name
#[async_trait]
pub trait WebSearcher: Send + Sync {
    /// The provider id (`brave`, `tavily`, …) used for routing and listing.
    fn name(&self) -> &str;

    /// Run one search and return ranked results.
    async fn search(&self, request: SearchRequest) -> Result<SearchResults>;
}

#[cfg(test)]
mod tests {
    use super::{strip_workspace_key, workspace_object_key, SearchRequest, SearchResults};
    use crate::id::WorkspaceId;

    #[test]
    fn search_request_defaults_and_builders() {
        let req = SearchRequest::new("rust async").limit(10).provider("brave");
        assert_eq!(req.query, "rust async");
        assert_eq!(req.limit, 10);
        assert_eq!(req.provider.as_deref(), Some("brave"));
        assert!(!req.include_raw_content);
        // A bare query string deserializes with the limit default applied.
        let bare: SearchRequest = serde_json::from_str(r#"{"query":"hi"}"#).unwrap();
        assert_eq!(bare.limit, 5);
        assert!(bare.provider.is_none());
    }

    #[test]
    fn search_results_round_trip_omits_empty_optionals() {
        let res = SearchResults {
            query: "q".into(),
            provider: "brave".into(),
            results: vec![],
            answer: None,
        };
        let json = serde_json::to_string(&res).unwrap();
        // `answer` is skipped when None, keeping the payload (and context) small.
        assert!(!json.contains("answer"), "got: {json}");
        let back: SearchResults = serde_json::from_str(&json).unwrap();
        assert_eq!(back, res);
    }

    #[test]
    fn workspace_object_key_namespaces_and_round_trips() {
        let ws = WorkspaceId::new();
        let physical = workspace_object_key(ws, "docs/readme.md");
        assert_eq!(physical, format!("{ws}/docs/readme.md"));
        // A leading slash on the user key is normalized (no empty segment).
        assert_eq!(workspace_object_key(ws, "/docs/x"), format!("{ws}/docs/x"));
        // Round-trips back to the user-facing key.
        assert_eq!(strip_workspace_key(ws, &physical), "docs/readme.md");
    }

    #[test]
    fn distinct_workspaces_never_collide_on_the_same_key() {
        let a = WorkspaceId::new();
        let b = WorkspaceId::new();
        // The same user key in two workspaces maps to two distinct physical keys,
        // so one tenant's blob can never read/overwrite another's (SOUL §18).
        assert_ne!(
            workspace_object_key(a, "report.txt"),
            workspace_object_key(b, "report.txt")
        );
        // A physical key only strips under its OWN workspace prefix.
        let pa = workspace_object_key(a, "report.txt");
        assert_eq!(strip_workspace_key(a, &pa), "report.txt");
        assert_eq!(
            strip_workspace_key(b, &pa),
            pa,
            "foreign prefix left intact"
        );
    }
}
