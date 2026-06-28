//! catalerum-automation — the durable trigger→condition→action engine (SOUL §11).
//!
//! Automation **definitions** live in Postgres (`catalerum-store`'s `automations`
//! table) as JSON `triggers` / `condition` / `actions`; this crate gives those
//! specs their **typed meaning**. [`AutomationSpec`] is the parsed, validated form
//! — a list of typed [`Trigger`]s, an optional condition predicate, and a list of
//! typed [`Action`]s — and [`validate`] is the cheap authoring-time check a
//! create/update path runs before persisting.
//!
//! Triggers are fully typed (the §11 set, by `kind`). Actions carry a typed
//! [`ActionKind`] discriminant plus opaque `params` — most action payloads are
//! interpreted by their executor in a later slice; the one payload that matters
//! structurally now, an `LlmAgent`'s restricted tool/skill set (§11/§19/§23), has
//! a typed accessor ([`Action::as_llm_agent`]).
//!
//! [`AutomationEngine`] drives execution through three narrow ports:
//! [`ExecutionState`] for the durable run journal and authority snapshot,
//! [`ActionRunner`] for typed side effects, and [`CodeRunner`] for sandboxed code.
//! The engine itself is storage-neutral; `PostgresExecutionState` is an edge
//! adapter preserving Postgres as truth. Trigger registration, job dispatch, API,
//! and the web editor stay outside the execution plane.

#![forbid(unsafe_code)]

pub mod articles;
pub mod catalog;
pub mod engine;
pub mod executor;
pub mod graph;
pub mod layout;
#[cfg(feature = "postgres")]
pub mod postgres;
pub mod schedule;
pub mod trigger;

pub use articles::{articles, Article};
pub use catalog::{catalog, NodeDoc, NodeParam};
pub use engine::{AutomationEngine, ExecutionError, ExecutionState};
pub use executor::{ActionOutcome, ActionRunner, CodeRunner, FailCodeRunner};
pub use graph::{Edge, Graph, Node, NodeKind, Position};
pub use layout::{apply_auto_layout, auto_layout_positions};
#[cfg(feature = "postgres")]
pub use postgres::{execute, execute_for_job, PostgresExecutionState};
pub use schedule::{due_in_window, due_occurrence, validate as validate_cron, CronError};
pub use trigger::{
    automation_matches, calendar_event_filter_matches, matching_automations, TriggerEvent,
};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use catalerum_core::Automation;

/// A typed automation trigger (SOUL §11). Deserialized from the stored
/// `{ "kind": "…", … }` JSON; the `kind` tag selects the variant.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Trigger {
    /// Fire ahead of a calendar event (§8/§10). `lead`/`filter` predicates are
    /// kept opaque until the schedule/predicate language lands.
    CalendarEvent {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lead: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filter: Option<Value>,
    },
    /// Fire on a storage-object event in a bucket/prefix (§9). `event` is the
    /// change kind — `"created"` (a new object), `"updated"` (an existing object's
    /// bytes changed), or `"deleted"` (an object removed) — matched exactly against
    /// what the storage layer fires (uploads, out-of-band drops the watch worker
    /// picks up, deletions). `prefix` narrows by key prefix; `extensions` narrows by
    /// file type.
    StorageObject {
        event: String,
        bucket: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefix: Option<String>,
        /// Optional file-extension allow-list — the object's key must end in one of
        /// these for the trigger to fire (e.g. `["docx", "xlsx", "pptx", "odt"]` to
        /// target office documents). Case-insensitive, and each entry's leading dot
        /// is optional (`"docx"` and `".docx"` both match `report.docx`). **Empty**
        /// (the default) imposes no constraint, so a trigger authored without it
        /// keeps matching every key — a downstream `Condition` can still refine.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        extensions: Vec<String>,
    },
    /// Fire on a cron schedule. `tz` is the IANA timezone the cron is evaluated in
    /// (e.g. `"America/New_York"`); absent → UTC (SOUL §11).
    Schedule {
        cron: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tz: Option<String>,
    },
    /// Fire on an inbound webhook at `path`.
    Webhook { path: String },
    /// Fire on a periodic **Datalog** graph query (SOUL §6.3/§18). `query` is a
    /// program in the safe in-process query language ([`catalerum_logic`]); the
    /// automation fires when the query returns ≥1 row. Scope is structural — the
    /// language cannot name a workspace, so cross-tenant reach is impossible by
    /// construction (there is no raw-Cypher escape hatch here anymore).
    GraphQuery {
        query: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        every: Option<Value>,
    },
    /// Fire on an inbound channel message (§25).
    ChannelMessage {
        channel: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filter: Option<Value>,
    },
    /// **Collect email** (SOUL §10/§28) — a *source* trigger you fill with an
    /// email [`Connection`](catalerum_core::model::Connection) id: it polls the
    /// provider on its `every` cadence (heavy I/O off the 60s clock, via the §6.2
    /// durable job queue) and **fires one automation run per new external
    /// message** (at-least-once), carrying that message as the run's trigger event
    /// for a downstream `WriteEmail`/`LlmAgent` to act on. So ingest *is* the head
    /// of a graph, not a background poller — adding a connection provisions
    /// nothing; until a Collect automation exists the source is dormant.
    ///
    /// - `connection` — the email connection id to pull from (required).
    /// - `mailbox` — narrow to a folder by name/external id (default: the
    ///   connection's configured mailbox).
    /// - `filter` — optional predicate over the message (same interim
    ///   `{"sender"|"subject"|"body": substring}` convention as `CalendarEvent`).
    /// - `commit_on` — id of a downstream write node; the connection's sync cursor
    ///   advances for a message **only once that node `Succeeded`** for the run (a
    ///   `Condition`-`Skipped` write counts as intentionally committed). Unset =
    ///   fire-and-forget: the run still happens and the cursor advances regardless.
    /// - `backfill_window` — bound the first poll (`{"days": N}`); default: only
    ///   mail newer than the automation's creation.
    /// - `every` — poll cadence (SOUL §29): a bare number of minutes, `{"minutes": N}`
    ///   or `{"seconds": N}`, or a duration string like `"30s"`/`"5m"`/`"1h"`/`"1h30m"`/`"1y"`.
    ///   Clamped to `[60s, 1 year]` (a too-small value clamps up to the scheduler tick
    ///   rather than erroring). Unset → the shared tick (poll once per 60s tick).
    ///
    /// Supersedes the old `MailReceived` reactor (no background poller ⇒ nothing
    /// for a reactor to fire on).
    CollectEmail {
        connection: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mailbox: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filter: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        commit_on: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        backfill_window: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        every: Option<Value>,
    },
    /// **Collect calendar** (SOUL §8/§10) — the calendar twin of [`CollectEmail`]:
    /// polls a calendar [`Connection`](catalerum_core::model::Connection) and fires
    /// one run per new external event, for a downstream `WriteEvent` to persist.
    /// Distinct from `CalendarEvent`, which is a *reminder* fired ahead of an
    /// already-**stored** event. `calendar` narrows to one calendar by name/external
    /// id; the other fields mean what they do on [`CollectEmail`].
    CollectCalendar {
        connection: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        calendar: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filter: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        commit_on: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        backfill_window: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        every: Option<Value>,
    },
    /// **Collect SQL rows** (SOUL §11/§19) — the external-database twin of
    /// [`CollectEmail`]: polls an external **Postgres**
    /// [`Connection`](catalerum_core::model::Connection) on its `every` cadence and
    /// **fires one automation run per newly-inserted row** in every table matching
    /// the `tables` wildcard pattern. The row rides the run's trigger event
    /// (`trigger.row.<column>`), so a downstream `LlmAgent`/`Code`/write node acts
    /// per row — the loop that lets an emerged UI (or anything else) `INSERT` via
    /// `sql_query` and have an automation react.
    ///
    /// - `connection` — the external Postgres connection id to poll (required).
    /// - `tables` — a wildcard table pattern (required): `*` matches any run of
    ///   characters, e.g. `orders_*`, `app_*_events`, or a schema-qualified
    ///   `analytics.fact_*`. Unqualified patterns match in the connection's
    ///   configured schema (default `public`). Tables created **later** that match
    ///   are picked up automatically on their first poll.
    /// - `cursor_column` — the column whose ascending order defines "new" (an
    ///   auto-increment id or an insertion timestamp). Optional: when unset each
    ///   table auto-detects a sequence/identity integer column (preferring the
    ///   primary key), else a `created_at`-style timestamp column; a table with
    ///   neither is skipped with a warning.
    /// - `commit_on` / `every` — as on [`CollectEmail`]. The per-table cursor
    ///   advances for a row only once the `commit_on` node `Succeeded` for its run.
    ///
    /// The **first** poll of a table anchors its cursor at the current maximum
    /// (existing rows never fire — "new inserts" means new after wiring); only
    /// rows inserted past the anchor fire runs. Updates and deletes are invisible
    /// to this trigger by design — it watches inserts via a monotonic cursor.
    CollectSql {
        connection: String,
        tables: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cursor_column: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        commit_on: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        every: Option<Value>,
    },
    /// Fire when a Kanban task moves into `to_column` on `board` (§24).
    TaskMoved { board: String, to_column: String },
    /// Fire on an explicit **named signal** (SOUL §11/§12) — a push-driven trigger
    /// with no external source of its own: something *inside* catalerum emits a
    /// `{ "kind": "trigger", "name": … }` event and every enabled automation whose
    /// trigger names the same `name` runs. The motivating caller is an **emerged
    /// UI** (SOUL §12): a button/handler invokes the `fire_trigger` tool to run an
    /// automation on demand, so a UI can drive a backend workflow without the
    /// automation needing a webhook path or a schedule — the same signal is equally
    /// fireable from chat, an automation code node, or any other tool caller.
    /// Matching keys on `name` alone (exact, case-sensitive); the firing side may
    /// attach an opaque `payload` (carried on the run's trigger event, never part of
    /// matching) so the automation's downstream nodes / `LlmAgent` see the caller's
    /// context.
    Trigger { name: String },
}

impl Trigger {
    /// The stable `kind` discriminant (matching the stored JSON tag) — for
    /// indexing triggers by type when the engine registers them.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Trigger::CalendarEvent { .. } => "calendar_event",
            Trigger::StorageObject { .. } => "storage_object",
            Trigger::Schedule { .. } => "schedule",
            Trigger::Webhook { .. } => "webhook",
            Trigger::GraphQuery { .. } => "graph_query",
            Trigger::ChannelMessage { .. } => "channel_message",
            Trigger::CollectEmail { .. } => "collect_email",
            Trigger::CollectCalendar { .. } => "collect_calendar",
            Trigger::CollectSql { .. } => "collect_sql",
            Trigger::TaskMoved { .. } => "task_moved",
            Trigger::Trigger { .. } => "trigger",
        }
    }

    /// Whether this is a **collect** source trigger (`CollectEmail` /
    /// `CollectCalendar` / `CollectSql`) — the poll-driven ingest heads (SOUL
    /// §10/§28). These never match an ad-hoc [`TriggerEvent`](crate::TriggerEvent);
    /// the collect scanner drives them on a cadence.
    #[must_use]
    pub fn is_collect(&self) -> bool {
        matches!(
            self,
            Trigger::CollectEmail { .. }
                | Trigger::CollectCalendar { .. }
                | Trigger::CollectSql { .. }
        )
    }

    /// The connection id a collect trigger pulls from (`None` for any other kind).
    #[must_use]
    pub fn collect_connection(&self) -> Option<&str> {
        match self {
            Trigger::CollectEmail { connection, .. }
            | Trigger::CollectCalendar { connection, .. }
            | Trigger::CollectSql { connection, .. } => Some(connection.as_str()),
            _ => None,
        }
    }

    /// The downstream write node a collect trigger's cursor commit is gated on
    /// (`commit_on`), if set. `None` for a non-collect trigger or fire-and-forget
    /// collect (unset `commit_on`).
    #[must_use]
    pub fn commit_on(&self) -> Option<&str> {
        match self {
            Trigger::CollectEmail { commit_on, .. }
            | Trigger::CollectCalendar { commit_on, .. }
            | Trigger::CollectSql { commit_on, .. } => commit_on.as_deref(),
            _ => None,
        }
    }

    /// A collect trigger's optional poll-cadence predicate (`every`), kept opaque
    /// like `GraphQuery`'s — the collect scanner decodes it. `None` otherwise.
    #[must_use]
    pub fn collect_every(&self) -> Option<&Value> {
        match self {
            Trigger::CollectEmail { every, .. }
            | Trigger::CollectCalendar { every, .. }
            | Trigger::CollectSql { every, .. } => every.as_ref(),
            _ => None,
        }
    }

    /// A collect trigger's optional first-poll backfill bound (`backfill_window`),
    /// kept opaque. `None` otherwise.
    #[must_use]
    pub fn backfill_window(&self) -> Option<&Value> {
        match self {
            Trigger::CollectEmail {
                backfill_window, ..
            }
            | Trigger::CollectCalendar {
                backfill_window, ..
            } => backfill_window.as_ref(),
            _ => None,
        }
    }
}

/// The kind of a typed automation action (SOUL §11) — these are also the LLM's
/// tools when an `LlmAgent` action runs the §7 loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    /// Run the §7 agent loop with a restricted tool/skill set + grant.
    LlmAgent,
    /// Run a named [`AgentProfile`](catalerum_core::model::AgentProfile) (§19) — a
    /// durable scoped agent — as an action. Its grant must be ⊆ this automation's.
    RunProfile,
    /// Run a named [`Skill`](catalerum_core::model::Skill) (§23) through the §7 agent
    /// loop: the skill's `instructions_md` seeds the system prompt and its `tools`
    /// confine the loop. Capability-gated `skill:use@<name>` (like the `use_skill`
    /// tool, §19); the skill's tools are still bounded by the automation's grant on
    /// dispatch. The first-class, named counterpart to wiring an `LlmAgent` by hand.
    RunSkill,
    CreateEvent,
    UpdateEvent,
    /// Persist a **collected email** (SOUL §10/§28) — take the message from an
    /// upstream `CollectEmail` trigger and **idempotently upsert it into Postgres**
    /// by `(mailbox_id, uid)`, enqueuing the §10 `chunk → embed → project`
    /// pipeline. This is the **only** thing that lands external mail in the store
    /// (read-only w.r.t. the provider, §14); gated `email:write`.
    WriteEmail,
    /// Persist a **collected calendar event** (SOUL §8/§10) — the calendar twin of
    /// [`WriteEmail`]: idempotent upsert by `(calendar_id, uid)` + enqueue the event
    /// graph projection. Gated `calendar:write`.
    WriteEvent,
    /// Record a classifier verdict on a **stored** email (SOUL §11/§28): set the
    /// email's free-text `labels` (e.g. an `LlmAgent` decides "receipt"/"urgent").
    /// Idempotent (a full replace of the label set). Gated `email:write`.
    LabelEmail,
    /// Mark a **stored** email read (SOUL §11/§28) — set its local `seen` flag
    /// (pass `"unread": true` to clear it instead). **Local only**: the provider's
    /// mailbox is never written (§14), so a provider re-sync may overwrite it.
    /// Targets the message an upstream `WriteEmail` persisted, resolved from the
    /// trigger item's `(mailbox_id, uid)` like [`ActionKind::LabelEmail`].
    /// Idempotent (setting a single flag). Gated `email:write`.
    MarkEmailRead,
    MoveObject,
    WriteObject,
    /// Run a shell command / code via the Executor (§20).
    RunCommand,
    /// Stand up an interactive terminal (PTY) session that persists across graph
    /// nodes (SOUL §20). Its output carries a `session_id` the downstream
    /// `terminal_*` nodes reference (e.g. `{{ inputs.<this-node-id>.session_id }}`).
    OpenTerminal,
    /// Write input to a terminal session opened upstream — a command line, with a
    /// trailing `\n` to run it (SOUL §20).
    TerminalWrite,
    /// Drain the output a terminal session has produced (SOUL §20). Set `wait_secs`
    /// to block until the command's output settles, so a one-shot read captures it.
    TerminalRead,
    /// Snapshot a terminal session's working dir to object storage (SOUL §20) — how
    /// an ephemeral terminal's converted files are kept. Reuses the dir→storage sync.
    PersistTerminal,
    /// Close a terminal session and free its PTY / process (SOUL §20).
    CloseTerminal,
    CreateNote,
    EditNote,
    CreateTask,
    MoveTask,
    /// Publish automation output as the first assistant message in a new chat
    /// thread. The thread is visible in the ordinary conversation list and can be
    /// continued by a user. Gated `conversation:write`.
    CreateChatThread,
    /// Send a message to a channel (§25).
    Notify,
    Summarize,
    /// (Re-)index a document source into the derived vector index (SOUL §6.4/§10):
    /// enqueue the idempotent embed→upsert pipeline for a note / stored object /
    /// memory so its chunks become semantically searchable (`search_semantic`).
    IndexDocument,
    /// Bulk (re-)index every uploaded file under a bucket / key-prefix (SOUL §6.4/§10)
    /// — the batch companion to [`ActionKind::IndexDocument`]. Enqueues the idempotent
    /// embed pipeline for each catalogued object under the prefix (e.g. a whole wiki
    /// folder). Backed by the `reindex_objects` tool.
    ReindexObjects,
    /// Fetch a web page (SOUL §27) — GET a URL through the web-fetch backend and
    /// return its content (Markdown by default, or `html`/`text`). The deterministic
    /// graph twin of an LLM `fetch_url` call. Backed by the `fetch_url` tool; gated
    /// `web:read` and needs a configured fetch backend (`fetcher.is_some()`), like
    /// `POST /fetch`. The SSRF guard runs in the backend regardless.
    FetchUrl,
    /// Search the web (SOUL §27) — return ranked results (title, url, snippet) for
    /// `queries`. The deterministic graph twin of an LLM `web_search` call; chain a
    /// result `url` into a downstream `FetchUrl` node. Backed by the `web_search`
    /// tool, registered only when a search provider is configured (`[search]`);
    /// gated `web:search`. `provider` picks a specific engine; omit for the default.
    WebSearch,
    /// Convert HTML to clean Markdown (or plain text) — a **pure** data transform
    /// (SOUL §27), no network and no capability. Feed it an upstream `FetchUrl`
    /// node's `html` (or any HTML string) to strip boilerplate down to readable
    /// content. Backed by the always-available `html_to_markdown` tool.
    HtmlToMarkdown,
    /// Extract parts of an HTML document by CSS selector — a **pure** data transform,
    /// no network and no capability. Returns each matched element's text / inner /
    /// outer HTML / attribute, so a graph can scrape a specific field out of a fetched
    /// page. Backed by the always-available `extract_html` tool.
    ExtractHtml,
    /// Run a parameterized SQL statement against an external Postgres connection
    /// the workspace owns (SOUL §11/§19). Backed by the `sql_query` tool; reads
    /// need `db:read@<conn>`, writes `db:write@<conn>`. Params: `connection`,
    /// `sql` (with `$1`/`$2` placeholders), `params` (values array), optional
    /// `mode` (`read`/`write`) + `max_rows`. The deterministic graph twin of a
    /// chat `sql_query` call — chain it inside a `ForEach` to write one row per
    /// collected item (e.g. a news article).
    SqlQuery,
    Webhook,
}

impl ActionKind {
    /// Whether re-executing this action on an at-least-once **redelivery** of an
    /// already-processed collect item (SOUL §11/§29) is safe — i.e. it is a
    /// keyed/idempotent write, a pure transform, or a read, so running it again
    /// yields no duplicate side effect.
    ///
    /// This closes the §29 "idempotent redelivery" hole: when a run is a redelivery
    /// (an upstream `WriteEmail`/`WriteEvent` reports the item was **already stored**
    /// — `newly_written: false` — or the firing event carries `redelivery: true`),
    /// the DAG executor auto-**Skips** every *non-idempotent* action node so it
    /// doesn't double-fire: no double-spent `LlmAgent`/`RunSkill`/`Summarize` tokens,
    /// no re-sent `Notify`/`Webhook`, no re-run `LabelEmail`/`RunCommand`, no
    /// duplicated `CreateNote`/`CreateTask`/`CreateEvent`/`CreateChatThread`. An
    /// **idempotent** action still runs: the write itself *must* run so its success
    /// advances the collect cursor (`commit_on`), and re-running a keyed upsert /
    /// pure transform / read is harmless. A node can force a non-idempotent action
    /// to run anyway with an action param `"rerun_on_redelivery": true`.
    ///
    /// Conservative by construction — only clearly re-run-safe kinds return `true`;
    /// anything with an external, costly, or duplicating effect returns `false` (and
    /// so is skipped on a redelivery). When in doubt a kind is treated as
    /// non-idempotent, because failing to skip merely wastes work (the status quo),
    /// while wrongly skipping a needed write could drop data — but the executor only
    /// *sets* the redelivery flag from an upstream write, so a keyed write is never
    /// cascaded off.
    #[must_use]
    pub fn is_idempotent(self) -> bool {
        matches!(
            self,
            // Keyed upserts — idempotent by (mailbox_id, uid) / (calendar_id, uid) /
            // (bucket, key); the collect writes MUST run to commit their cursor.
            ActionKind::WriteEmail
                | ActionKind::WriteEvent
                | ActionKind::WriteObject
                // Setting/clearing one stored flag — re-running is a no-op.
                | ActionKind::MarkEmailRead
                // Idempotent (re-)projection into the derived vector index.
                | ActionKind::IndexDocument
                | ActionKind::ReindexObjects
                // Pure data transforms — no side effect, no network, no capability.
                | ActionKind::HtmlToMarkdown
                | ActionKind::ExtractHtml
                // Read-only fetches — re-reading is safe, and downstream may need the
                // output; a redelivery is rare, so the repeat read is negligible.
                | ActionKind::FetchUrl
                | ActionKind::WebSearch
        )
    }
}

/// A typed automation action: a known [`ActionKind`] plus its action-specific
/// `params` (kept as JSON — most action payloads are interpreted by their
/// executor in a later slice). The structurally-significant payload now,
/// `LlmAgent`'s restricted tool/skill set (§11/§19/§23), has a typed accessor.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Action {
    /// Which action to run.
    pub kind: ActionKind,
    /// Action-specific parameters (everything besides `kind`).
    #[serde(flatten)]
    pub params: Map<String, Value>,
}

impl Action {
    /// The restricted agent spec for an `LlmAgent` action (§11/§19/§23): the
    /// system prompt, model, and the tool/skill subset the agent loop is confined
    /// to. `None` for any other kind (or if the params don't parse).
    #[must_use]
    pub fn as_llm_agent(&self) -> Option<LlmAgent> {
        if self.kind != ActionKind::LlmAgent {
            return None;
        }
        serde_json::from_value(Value::Object(self.params.clone())).ok()
    }

    /// The typed payload for a `RunProfile` action (§11/§19): which profile to run
    /// and an optional explicit input. `None` for any other kind (or if the params
    /// don't parse).
    #[must_use]
    pub fn as_run_profile(&self) -> Option<RunProfile> {
        if self.kind != ActionKind::RunProfile {
            return None;
        }
        serde_json::from_value(Value::Object(self.params.clone())).ok()
    }

    /// The typed payload for a `RunSkill` action (§11/§23): which skill to run, plus
    /// optional `input`/`model`/`output` overrides. `None` for any other kind (or if
    /// the params don't parse).
    #[must_use]
    pub fn as_run_skill(&self) -> Option<RunSkill> {
        if self.kind != ActionKind::RunSkill {
            return None;
        }
        serde_json::from_value(Value::Object(self.params.clone())).ok()
    }
}

/// The typed payload of an `LlmAgent` action (SOUL §11): runs the §7 loop with a
/// restricted tool/skill set under the automation's grant. Empty `tools`/`skills`
/// mean "no restriction expressed here" (the grant still bounds the agent).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LlmAgent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Reasoning ("thinking") effort the loop requests for reasoning-capable models:
    /// a free-form gateway token (`"low" | "medium" | "high" | "xhigh" | "max"`),
    /// passed through to the model. Absent → no reasoning requested (provider default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    /// Desired output shape: `"json"` steers the model to emit only a JSON value
    /// (parsed into the step's `data` field for downstream nodes); absent / any
    /// other value is plain text. Lets an agent feed structured data into further
    /// automation steps (SOUL §11).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

impl LlmAgent {
    /// Whether this agent is steered to emit JSON (`output == "json"`).
    #[must_use]
    pub fn wants_json(&self) -> bool {
        self.output.as_deref() == Some("json")
    }
}

/// The typed payload of a `RunProfile` action (SOUL §11/§19): run the named
/// [`AgentProfile`](catalerum_core::model::AgentProfile) — a durable scoped agent —
/// under its own grant, which the runner verifies is ⊆ this automation's grant
/// (attenuation). The optional `input` is the user turn the profile runs on; absent,
/// the runner derives it from the firing trigger.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RunProfile {
    /// Name of the agent profile to run (workspace-scoped).
    pub profile: String,
    /// Explicit input for the profile's user turn; absent → derived from the trigger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
}

/// The typed payload of a `RunSkill` action (SOUL §11/§23): run the named workspace
/// [`Skill`](catalerum_core::model::Skill) through the §7 agent loop — its
/// `instructions_md` becomes the system prompt and its `tools` confine the loop,
/// under the automation's grant (and a per-skill `skill:use@<name>` gate). The user
/// turn is the explicit `input`, else the firing channel message's text, else a
/// description of the trigger. `model` and `output` — the `LlmAgent` knobs a skill
/// doesn't itself carry — are optional.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RunSkill {
    /// Name of the skill to run (workspace-scoped).
    pub skill: String,
    /// Explicit user turn for the skill; absent → derived from the trigger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    /// Model override (a skill carries no model of its own); absent → the configured
    /// default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Desired output shape: `"json"` steers the model to emit only a JSON value
    /// (parsed into the step's `data` field for downstream nodes, as on `LlmAgent`);
    /// absent / any other value is plain text (SOUL §11).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

impl RunSkill {
    /// Whether this skill run is steered to emit JSON (`output == "json"`).
    #[must_use]
    pub fn wants_json(&self) -> bool {
        self.output.as_deref() == Some("json")
    }
}

/// The parsed, validated form of a stored [`Automation`] (SOUL §11): typed
/// triggers, an optional condition predicate (kept as JSON — the predicate
/// language is defined in a later slice), and typed actions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AutomationSpec {
    pub triggers: Vec<Trigger>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<Value>,
    pub actions: Vec<Action>,
}

impl AutomationSpec {
    /// Parse + validate a stored [`Automation`]'s JSON triggers/condition/actions
    /// into their typed form (SOUL §11).
    ///
    /// # Errors
    /// - [`SpecError::Trigger`] / [`SpecError::Action`] if a spec's `kind` is
    ///   unknown or a required field is missing/mistyped.
    /// - [`SpecError::NoTriggers`] / [`SpecError::NoActions`] if either list is
    ///   empty — an automation that can never fire, or that fires into nothing, is
    ///   rejected.
    pub fn parse(automation: &Automation) -> Result<Self, SpecError> {
        Self::from_json(
            &automation.triggers,
            automation.condition.as_ref(),
            &automation.actions,
        )
    }

    /// Parse + validate raw JSON `triggers`/`condition`/`actions` parts into a
    /// typed [`AutomationSpec`] (SOUL §11) — the form a create/update REST surface
    /// has before it builds a stored row, so it can reject a malformed spec with a
    /// `400` instead of persisting garbage.
    ///
    /// # Errors
    /// See [`AutomationSpec::parse`].
    pub fn from_json(
        triggers: &[Value],
        condition: Option<&Value>,
        actions: &[Value],
    ) -> Result<Self, SpecError> {
        let triggers = triggers
            .iter()
            .map(|v| serde_json::from_value::<Trigger>(v.clone()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| SpecError::Trigger(e.to_string()))?;
        if triggers.is_empty() {
            return Err(SpecError::NoTriggers);
        }
        // Time-driven triggers are validated at authoring time rather than stored as
        // a definition the scheduler will only ever skip (SOUL §11): a `Schedule`'s
        // cron must be a valid 5-field expression, and a `GraphQuery`'s Datalog
        // program must parse and be safe/well-formed (§18/§6.3).
        for trigger in &triggers {
            match trigger {
                Trigger::Schedule { cron, tz } => {
                    crate::schedule::validate(cron, tz.as_deref())
                        .map_err(|e| SpecError::Trigger(e.to_string()))?;
                }
                Trigger::GraphQuery { query, .. } => {
                    catalerum_logic::validate(query)
                        .map_err(|e| SpecError::Trigger(e.to_string()))?;
                }
                // A collect source trigger must name a connection to pull from —
                // an empty `connection` is a definition the scanner would only ever
                // skip (SOUL §10/§28).
                Trigger::CollectEmail { connection, .. }
                | Trigger::CollectCalendar { connection, .. }
                    if connection.trim().is_empty() =>
                {
                    return Err(SpecError::Trigger(
                        "a collect trigger must name a non-empty `connection` to pull from"
                            .to_string(),
                    ));
                }
                _ => {}
            }
        }
        let actions = actions
            .iter()
            .map(|v| serde_json::from_value::<Action>(v.clone()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| SpecError::Action(e.to_string()))?;
        if actions.is_empty() {
            return Err(SpecError::NoActions);
        }
        Ok(AutomationSpec {
            triggers,
            condition: condition.cloned(),
            actions,
        })
    }
}

/// Validate that a stored [`Automation`] parses into a well-formed
/// [`AutomationSpec`] (SOUL §11) — the cheap authoring-time check a create/update
/// path runs before persisting; discards the parsed value.
///
/// # Errors
/// See [`AutomationSpec::parse`].
pub fn validate(automation: &Automation) -> Result<(), SpecError> {
    AutomationSpec::parse(automation).map(|_| ())
}

/// Why a stored [`Automation`] failed to parse into a typed [`AutomationSpec`].
#[derive(Debug, thiserror::Error)]
pub enum SpecError {
    /// A trigger spec had an unknown `kind` or a missing/mistyped field.
    #[error("invalid trigger: {0}")]
    Trigger(String),
    /// An action spec had an unknown `kind` or a missing/mistyped field.
    #[error("invalid action: {0}")]
    Action(String),
    /// The automation has no triggers — it could never fire.
    #[error("automation has no triggers")]
    NoTriggers,
    /// The automation has no actions — it would fire into nothing.
    #[error("automation has no actions")]
    NoActions,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse_trigger(v: Value) -> Result<Trigger, serde_json::Error> {
        serde_json::from_value(v)
    }

    #[test]
    fn triggers_round_trip_by_kind() {
        let t = parse_trigger(json!({ "kind": "schedule", "cron": "0 9 * * *" })).unwrap();
        assert_eq!(
            t,
            Trigger::Schedule {
                cron: "0 9 * * *".into(),
                tz: None
            }
        );
        assert_eq!(t.kind(), "schedule");
        // A timezone round-trips when present.
        let tzd = parse_trigger(
            json!({ "kind": "schedule", "cron": "0 9 * * *", "tz": "Europe/Berlin" }),
        )
        .unwrap();
        assert_eq!(
            tzd,
            Trigger::Schedule {
                cron: "0 9 * * *".into(),
                tz: Some("Europe/Berlin".into())
            }
        );

        let t =
            parse_trigger(json!({ "kind": "task_moved", "board": "sprint", "to_column": "done" }))
                .unwrap();
        assert_eq!(t.kind(), "task_moved");
        // Serializing re-emits the tag, so a stored→typed→stored round-trip is stable.
        assert_eq!(
            serde_json::to_value(&t).unwrap()["kind"],
            json!("task_moved")
        );
        assert!(
            matches!(t, Trigger::TaskMoved { board, to_column } if board == "sprint" && to_column == "done")
        );
    }

    #[test]
    fn unknown_trigger_kind_is_rejected() {
        assert!(parse_trigger(json!({ "kind": "telepathy" })).is_err());
    }

    #[test]
    fn schedule_without_its_required_field_is_rejected() {
        assert!(parse_trigger(json!({ "kind": "schedule" })).is_err());
        assert!(parse_trigger(json!({ "kind": "task_moved", "board": "b" })).is_err());
    }

    #[test]
    fn schedule_with_an_invalid_cron_is_rejected_at_authoring() {
        let actions = vec![json!({ "kind": "summarize" })];
        // A valid 5-field cron passes.
        assert!(AutomationSpec::from_json(
            &[json!({ "kind": "schedule", "cron": "0 9 * * *" })],
            None,
            &actions,
        )
        .is_ok());
        // Garbage / out-of-range crons are rejected before persisting.
        assert!(AutomationSpec::from_json(
            &[json!({ "kind": "schedule", "cron": "not a cron" })],
            None,
            &actions,
        )
        .is_err());
        assert!(AutomationSpec::from_json(
            &[json!({ "kind": "schedule", "cron": "99 99 99 99 99" })],
            None,
            &actions,
        )
        .is_err());
    }

    #[test]
    fn graph_query_trigger_uses_the_datalog_query_field() {
        // Decodes the `query` field and round-trips with the graph_query tag.
        let t = parse_trigger(json!({ "kind": "graph_query", "query": "?- note(N)." })).unwrap();
        assert_eq!(t.kind(), "graph_query");
        assert!(matches!(&t, Trigger::GraphQuery { query, .. } if query == "?- note(N)."));
        assert_eq!(
            serde_json::to_value(&t).unwrap()["kind"],
            json!("graph_query")
        );

        // A legacy raw-Cypher payload (retired `cypher` field) no longer decodes —
        // `query` is required (hard replace, no serde alias).
        assert!(
            parse_trigger(json!({ "kind": "graph_query", "cypher": "MATCH (n) RETURN n" }))
                .is_err()
        );
    }

    #[test]
    fn graph_query_datalog_is_validated_at_authoring() {
        let actions = vec![json!({ "kind": "summarize" })];
        // A valid Datalog program is accepted.
        assert!(AutomationSpec::from_json(
            &[json!({ "kind": "graph_query", "query": "?- note(N), prop(N, \"title\", T)." })],
            None,
            &actions,
        )
        .is_ok());
        // Raw Cypher is not a valid program → rejected before persisting.
        assert!(AutomationSpec::from_json(
            &[json!({ "kind": "graph_query", "query": "MATCH (n) RETURN n" })],
            None,
            &actions,
        )
        .is_err());
        // An unsafe program (unbound head variable) → rejected.
        assert!(AutomationSpec::from_json(
            &[json!({ "kind": "graph_query", "query": "p(X, Y) :- node(X, \"Note\").\n?- p(X, Y)." })],
            None,
            &actions,
        )
        .is_err());
    }

    #[test]
    fn action_carries_kind_plus_opaque_params() {
        let a: Action =
            serde_json::from_value(json!({ "kind": "notify", "channel": "matrix" })).unwrap();
        assert_eq!(a.kind, ActionKind::Notify);
        assert_eq!(a.params["channel"], json!("matrix"));
        assert!(a.as_llm_agent().is_none());

        let bare: Action = serde_json::from_value(json!({ "kind": "summarize" })).unwrap();
        assert_eq!(bare.kind, ActionKind::Summarize);
        assert!(bare.params.is_empty());
        // Round-trips with the kind tag preserved.
        assert_eq!(
            serde_json::to_value(&bare).unwrap(),
            json!({ "kind": "summarize" })
        );
    }

    #[test]
    fn llm_agent_action_exposes_its_restricted_set() {
        let a: Action = serde_json::from_value(json!({
            "kind": "llm_agent",
            "system": "be brief",
            "model": "echo",
            "tools": ["create_note", "search_semantic"],
            "skills": ["summarize"]
        }))
        .unwrap();
        let agent = a.as_llm_agent().expect("llm_agent payload");
        assert_eq!(agent.system.as_deref(), Some("be brief"));
        assert_eq!(agent.model.as_deref(), Some("echo"));
        assert_eq!(
            agent.tools,
            vec!["create_note".to_string(), "search_semantic".to_string()]
        );
        assert_eq!(agent.skills, vec!["summarize".to_string()]);
    }

    #[test]
    fn unknown_action_kind_is_rejected() {
        assert!(serde_json::from_value::<Action>(json!({ "kind": "explode" })).is_err());
    }

    #[test]
    fn redelivery_idempotency_classification_is_conservative() {
        // The keyed writes MUST stay idempotent (they run on a redelivery to commit
        // the collect cursor) — regressing these would drop items.
        for k in [
            ActionKind::WriteEmail,
            ActionKind::WriteEvent,
            ActionKind::WriteObject,
            ActionKind::MarkEmailRead,
            ActionKind::IndexDocument,
            ActionKind::ReindexObjects,
            ActionKind::HtmlToMarkdown,
            ActionKind::ExtractHtml,
            ActionKind::FetchUrl,
            ActionKind::WebSearch,
        ] {
            assert!(
                k.is_idempotent(),
                "{k:?} must be re-run-safe on a redelivery"
            );
        }
        // The non-idempotent actions the §29 hole is about — these auto-skip on a
        // redelivery so they don't double-fire (tokens, notifies, labels, creates).
        for k in [
            ActionKind::LlmAgent,
            ActionKind::RunProfile,
            ActionKind::RunSkill,
            ActionKind::Summarize,
            ActionKind::LabelEmail,
            ActionKind::Notify,
            ActionKind::Webhook,
            ActionKind::CreateNote,
            ActionKind::CreateTask,
            ActionKind::CreateEvent,
            ActionKind::CreateChatThread,
            ActionKind::RunCommand,
        ] {
            assert!(!k.is_idempotent(), "{k:?} must be skipped on a redelivery");
        }
    }

    fn automation(triggers: Vec<Value>, actions: Vec<Value>) -> Automation {
        Automation {
            id: catalerum_core::AutomationId::new(),
            workspace_id: catalerum_core::WorkspaceId::new(),
            name: "a".into(),
            enabled: true,
            triggers,
            condition: None,
            actions,
            spec: None,
            grant_id: None,
        }
    }

    #[test]
    fn parse_validates_a_realistic_automation() {
        let a = automation(
            vec![json!({ "kind": "schedule", "cron": "0 9 * * *" })],
            vec![
                json!({ "kind": "llm_agent", "skills": ["weekly-review"] }),
                json!({ "kind": "summarize" }),
            ],
        );
        let spec = AutomationSpec::parse(&a).expect("parse");
        assert_eq!(spec.triggers.len(), 1);
        assert_eq!(spec.actions.len(), 2);
        assert_eq!(spec.triggers[0].kind(), "schedule");
        assert_eq!(
            spec.actions[0].as_llm_agent().unwrap().skills,
            vec!["weekly-review".to_string()]
        );
        assert!(validate(&a).is_ok());
    }

    #[test]
    fn parse_typed_run_skill_payload() {
        let a = automation(
            vec![json!({ "kind": "schedule", "cron": "0 9 * * *" })],
            vec![json!({
                "kind": "run_skill",
                "skill": "triage-inbox",
                "input": "Triage the new mail",
                "output": "json"
            })],
        );
        let spec = AutomationSpec::parse(&a).expect("parse");
        let skill = spec.actions[0].as_run_skill().expect("run_skill payload");
        assert_eq!(skill.skill, "triage-inbox");
        assert_eq!(skill.input.as_deref(), Some("Triage the new mail"));
        assert!(skill.wants_json());
        // A wrong-kind accessor yields `None` (the discriminant guards it).
        assert!(spec.actions[0].as_run_profile().is_none());
        assert!(validate(&a).is_ok());
    }

    #[test]
    fn parse_preserves_the_opaque_condition_predicate() {
        let mut a = automation(
            vec![json!({ "kind": "schedule", "cron": "0 9 * * *" })],
            vec![json!({ "kind": "summarize" })],
        );
        let cond = json!({ "predicate": "has_tag", "tag": "urgent" });
        a.condition = Some(cond.clone());
        let spec = AutomationSpec::parse(&a).expect("parse");
        assert_eq!(
            spec.condition,
            Some(cond),
            "parse preserves the condition verbatim"
        );
    }

    #[test]
    fn from_json_validates_raw_parts_directly() {
        // The REST create/update path: validate the request body before persisting.
        let triggers = vec![json!({ "kind": "webhook", "path": "/hook" })];
        let actions = vec![json!({ "kind": "summarize" })];
        let spec = AutomationSpec::from_json(&triggers, None, &actions).expect("valid parts");
        assert_eq!(spec.triggers[0].kind(), "webhook");
        // Bad parts surface the same errors as parse.
        assert!(matches!(
            AutomationSpec::from_json(&[], None, &actions),
            Err(SpecError::NoTriggers)
        ));
        assert!(matches!(
            AutomationSpec::from_json(&[json!({ "kind": "schedule" })], None, &actions),
            Err(SpecError::Trigger(_))
        ));
    }

    #[test]
    fn empty_triggers_or_actions_are_rejected() {
        let no_trig = automation(vec![], vec![json!({ "kind": "summarize" })]);
        assert!(matches!(
            AutomationSpec::parse(&no_trig),
            Err(SpecError::NoTriggers)
        ));

        let no_act = automation(vec![json!({ "kind": "webhook", "path": "/hook" })], vec![]);
        assert!(matches!(
            AutomationSpec::parse(&no_act),
            Err(SpecError::NoActions)
        ));
    }

    #[test]
    fn a_bad_spec_in_a_stored_automation_surfaces_as_a_spec_error() {
        let bad_trigger = automation(
            vec![json!({ "kind": "nope" })],
            vec![json!({ "kind": "summarize" })],
        );
        assert!(matches!(
            AutomationSpec::parse(&bad_trigger),
            Err(SpecError::Trigger(_))
        ));

        let bad_action = automation(
            vec![json!({ "kind": "webhook", "path": "/h" })],
            vec![json!({ "kind": "explode" })],
        );
        assert!(matches!(
            AutomationSpec::parse(&bad_action),
            Err(SpecError::Action(_))
        ));
    }
}
