//! Emerged UIs — the declarative, AI-authored component model (the "emerged UI"
//! feature).
//!
//! catalerum-web is Leptos compiled to wasm32, so the LLM cannot author Rust at
//! runtime. An emerged UI is instead a **typed, closed-vocabulary JSON component
//! tree** ([`UiSpec`]) that the AI creates and edits through tools, persisted as
//! one [`UiDefinition`](crate::model::UiDefinition) JSONB row per workspace and
//! rendered by a single generic Leptos interpreter — the same
//! spec-then-interpret pattern the automation flow editor already uses.
//!
//! Three load-bearing rules keep this safe and forward-compatible:
//! 1. **Closed enums.** [`NodeKind`], [`EventName`], [`Handler`], [`ClientOp`],
//!    [`ValidationKind`] and [`UiPatchOp`] reject unknown variants at
//!    deserialize. They are therefore **append-only**: never rename or remove a
//!    variant without a blob-rewriting migration gated on
//!    [`UiDefinition::spec_version`](crate::model::UiDefinition) — a closed enum
//!    in JSONB otherwise silently fails to load older specs.
//! 2. **No raw HTML/JS in the tree.** The only escape hatches are the `markdown`
//!    kind (rendered through the escape-safe markdown renderer) and named Boa
//!    `scripts` (server-side, sandboxed). [`validate_ui_spec`] enforces this.
//! 3. **Logic is server-side.** The client interpreter evaluates only `{{path}}`
//!    interpolation, single-path `show_if`, and `for_each`. Anything richer is a
//!    named Boa script handler run on the server.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::model::Map;

// ---------------------------------------------------------------------------
// The persisted spec
// ---------------------------------------------------------------------------

/// A complete emerged-UI definition: one or more views, the seed state, derived
/// (`computed`) values, and the named Boa `scripts` handlers reference.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UiSpec {
    /// The [`UiView::id`] shown on mount.
    pub default_view: String,
    /// The views of this mini-app (at least one).
    pub views: Vec<UiView>,
    /// Seeds the client-side transient state object on mount.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub initial_state: Map,
    /// Read-only derived values, each produced by a named [`ScriptDef`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub computed: Vec<ComputedDef>,
    /// Named Boa scripts referenced by [`Handler::Script`], [`ComputedDef`] and
    /// [`ValidationKind::Script`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub scripts: BTreeMap<String, ScriptDef>,
    /// For a **sub-app** of a shell App suite: the shell's ui id (UUID string).
    ///
    /// A shell App embeds sub-apps via [`NodeKind::AppRef`] nodes; a sub-app that
    /// names the shell here (and is in turn `app_ref`-referenced by it — mutual,
    /// server-verified opt-in) shares the shell's durable `app_data_*` namespace,
    /// so e.g. a "browse" and an "edit" sub-app operate on the same stored rows.
    /// Unset = a standalone App with its own namespace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_app: Option<String>,
}

/// One view (screen) of a multi-view emerged UI.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UiView {
    /// Stable, unique-within-the-spec view id (the [`ClientOp::Navigate`] target).
    pub id: String,
    /// Human title (shown as the artifact/app header).
    pub title: String,
    /// The root node rendered for this view.
    pub root: UiNode,
}

/// A read-only derived value, exposed to bindings at `computed.<name>`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ComputedDef {
    /// The name exposed under `computed.<name>`.
    pub name: String,
    /// A key into [`UiSpec::scripts`].
    pub handler: String,
}

/// A named server-side Boa script. `source` is a function body run as
/// `(function(input){ <source> })(__catalerum_input__)`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScriptDef {
    /// The runtime; only `"javascript"`/`"js"` is supported (Boa).
    #[serde(default = "js_runtime")]
    pub runtime: String,
    /// The JavaScript function body.
    pub source: String,
}

fn js_runtime() -> String {
    "javascript".to_string()
}

// ---------------------------------------------------------------------------
// Nodes
// ---------------------------------------------------------------------------

/// A single node in the component tree. `id` is stable and unique within a
/// [`UiSpec`]: it is both the patch target ([`UiPatchOp`]) and the `<For>` key.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UiNode {
    /// Stable, unique-within-the-spec node id.
    pub id: String,
    /// The (closed) node kind.
    pub kind: NodeKind,
    /// Bindable, kind-specific properties (`label`, `placeholder`, `options`,
    /// `columns`, …). String values may contain `{{path}}` interpolations.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub props: Map,
    /// Child nodes (container kinds only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<UiNode>,
    /// Two-way value binding for input kinds: a state path like `form.email`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind: Option<String>,
    /// Conditional render: a single truthy state path, optional leading `!`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_if: Option<String>,
    /// Repeat this node once per element of a state array.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub for_each: Option<ForEach>,
    /// Event handlers keyed by [`EventName`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub events: BTreeMap<EventName, Handler>,
    /// Validation rules (input kinds only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub validate: Vec<ValidationRule>,
}

/// The closed vocabulary of node kinds. **Append-only** (see module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    // Layout / containers.
    Stack,
    Row,
    Grid,
    Card,
    Dialog,
    Tabs,
    Tab,
    /// A single-child wrapper that applies min/max width and height bounds.
    ConstrainedBox,
    /// A single-child wrapper that preserves a width-to-height ratio.
    AspectRatio,
    Divider,
    // Content.
    Text,
    Heading,
    Markdown,
    Image,
    Link,
    Badge,
    ProgressBar,
    // Data visualisation (leaf; read-only). Each reads a `data` prop (a literal
    // JSON array or a `{"$path":"…"}`/`{{path}}` reference into state) and draws
    // a self-contained, theme-tokened SVG. They carry no children/bind/events.
    PieChart,
    DonutChart,
    BarChart,
    LineChart,
    AreaChart,
    Sparkline,
    Gauge,
    RadarChart,
    Heatmap,
    // Inputs + actions.
    Button,
    TextInput,
    Textarea,
    NumberInput,
    DateInput,
    Select,
    RadioGroup,
    Checkbox,
    Slider,
    // Collections (leaf; read-only). Like the charts, each reads a `data` prop
    // (a literal array or a `{"$path":…}`/`{{path}}` reference into state) and
    // renders it — a bullet list / a column table. Rows needing per-item
    // interactivity use `for_each` instead.
    List,
    Table,
    // Timers (leaf; client-run). `timer` counts a `props.duration` (seconds;
    // number or `{{path}}`) down and fires its `complete` handler at zero;
    // `stopwatch` counts up. Both draw start/pause/reset controls unless
    // `props.controls` is false, and are addressable from any handler via the
    // `start_timer`/`pause_timer`/`reset_timer` ops.
    Timer,
    Stopwatch,
    // Composition. `view_ref` renders another view of THIS spec inline (a
    // reusable sub-view fragment; `props.view` names it). `app_ref` mounts a
    // whole OTHER emerged UI (`props.app` = its ui id) — the shell-app seam.
    ViewRef,
    AppRef,
    // (Later: Icon, Menu, MenuItem — append only.)
}

impl NodeKind {
    /// Whether this kind may hold `children`.
    #[must_use]
    pub fn is_container(self) -> bool {
        matches!(
            self,
            NodeKind::Stack
                | NodeKind::Row
                | NodeKind::Grid
                | NodeKind::Card
                | NodeKind::Dialog
                | NodeKind::Tabs
                | NodeKind::Tab
                | NodeKind::ConstrainedBox
                | NodeKind::AspectRatio
                | NodeKind::Button
        )
    }

    /// Whether this kind is an input (carries `bind`/`validate`).
    #[must_use]
    pub fn is_input(self) -> bool {
        matches!(
            self,
            NodeKind::TextInput
                | NodeKind::Textarea
                | NodeKind::NumberInput
                | NodeKind::DateInput
                | NodeKind::Select
                | NodeKind::RadioGroup
                | NodeKind::Checkbox
                | NodeKind::Slider
        )
    }

    /// Whether this kind may carry event handlers.
    #[must_use]
    pub fn is_interactive(self) -> bool {
        self.is_input() || matches!(self, NodeKind::Button | NodeKind::Dialog)
    }
}

/// A loop binding: render the node once per element of a state array.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ForEach {
    /// State path to an array, e.g. `tasks`.
    #[serde(rename = "in")]
    pub source: String,
    /// The per-iteration variable name (default `item`).
    #[serde(rename = "as", default = "default_item")]
    pub item: String,
    /// Optional index variable name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<String>,
    /// Optional per-item key path (for stable `<For>` keys).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Optional client-side row filter (live search boxes / category pickers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<ForEachFilter>,
    /// Additional client-side row filters, ANDed with `filter` (a row must pass
    /// every one) — e.g. a text search plus a category dropdown at once.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filters: Vec<ForEachFilter>,
    /// Optional windowing over the (filtered) rows: split them into fixed-size
    /// pages with prev/next controls, or reveal more as the user scrolls to the
    /// bottom (infinite scroll). Absent = render every row up to the row budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paginate: Option<Pagination>,
}

fn default_item() -> String {
    "item".to_string()
}

/// Client-side windowing over a [`ForEach`]'s (already-filtered) rows. Only the
/// current window is put in the DOM, so a long array stays cheap: [`PageMode`]
/// picks between numbered pages and grow-on-scroll. The whole array still lives
/// in state — this is purely a rendering window, no server round-trip.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pagination {
    /// Rows per page (in `paged` mode) or per scroll/"load more" increment (in
    /// `infinite` mode). Clamped to a sane range by the renderer; default 20.
    #[serde(default = "default_page_size")]
    pub page_size: usize,
    /// How the rows are windowed.
    #[serde(default)]
    pub mode: PageMode,
}

fn default_page_size() -> usize {
    20
}

/// How a [`Pagination`] reveals rows. **Append-only**.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PageMode {
    /// One fixed-size page at a time, with prev/next controls and a page
    /// indicator (the classic pager).
    #[default]
    Paged,
    /// Start with one page and reveal another each time the user scrolls the
    /// bottom sentinel into view (or clicks its "Load more" fallback).
    Infinite,
}

/// A declarative per-row filter on a [`ForEach`], evaluated client-side against
/// live state — so a search input bound to `filter.query`'s path narrows the
/// rendered rows on every keystroke with no server round-trip. A row passes when
/// the value at `path` (within the item; the whole item when unset) matches the
/// value at the `query` state path. A falsy query (empty string / null / absent)
/// disables the filter, so an untouched search box shows every row.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ForEachFilter {
    /// Path within each item to match against (e.g. `title`); unset = the item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// State path holding the query value (e.g. `search` — bind an input to it).
    pub query: String,
    /// How the row value must match the query.
    #[serde(default)]
    pub mode: FilterMode,
}

/// How a [`ForEachFilter`] row value must match its query. **Append-only**.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterMode {
    /// Case-insensitive substring match on the stringified values (search box).
    #[default]
    Contains,
    /// Exact JSON equality, with string/scalar coercion (category dropdown).
    Equals,
}

/// The (closed) set of UI events. **Append-only**.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventName {
    Click,
    Submit,
    Change,
    Input,
    Select,
    Open,
    Close,
    /// Fired by the client when the view holding this node becomes active
    /// (mount + navigate-to). Only a view's **root** node fires it; the natural
    /// use is a `tool`/`script` handler that pulls durable data (e.g.
    /// `app_data_list`) into state so an App opens populated.
    Load,
    /// Fired by the client exactly once when a running [`NodeKind::Timer`]
    /// reaches zero (only `timer` nodes may carry it).
    Complete,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// What an event does. The three product-level kinds are [`Handler::Ai`],
/// [`Handler::Tool`] and [`Handler::Script`]; [`Handler::Client`] carries no
/// authority and never reaches the server. **Append-only**.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Handler {
    /// Pure client-side ops (dialogs, view switch, local state). No round-trip.
    Client {
        /// The ops applied locally, in order.
        #[serde(default)]
        ops: Vec<ClientOp>,
    },
    /// Call back to the AI as a new chat turn carrying the event + state.
    Ai {
        /// Optional extra instruction prepended to the synthesized turn.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
        /// Whether to include the current transient state in the turn.
        #[serde(default = "default_true")]
        include_state: bool,
    },
    /// Invoke a registry tool (capability-gated, server allow-listed).
    Tool {
        /// The registered tool name.
        tool: String,
        /// Tool arguments; string values may contain `{{path}}` interpolations.
        #[serde(default, skip_serializing_if = "Map::is_empty")]
        args: Map,
        /// Optional state path to write the tool result into.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result_path: Option<String>,
        /// Client ops applied after a successful call.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        then: Vec<ClientOp>,
    },
    /// Run a named Boa script (server-side, sandboxed).
    Script {
        /// A key into [`UiSpec::scripts`].
        handler: String,
    },
}

fn default_true() -> bool {
    true
}

/// A client-applied state mutation, authored in a [`Handler::Client`] or
/// [`Handler::Tool::then`]. **Append-only**.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ClientOp {
    /// Set a state path. The value is a literal, a `{"$path":"a.b"}` copy from
    /// another (scope-resolved) state path, or a string carrying `{{path}}`
    /// references — a whole reference (`"{{item.id}}"`) yields the raw typed
    /// value, a mixed string splices display text (same rules as tool args).
    Set {
        /// Target state path.
        path: String,
        /// The value (a literal, `{"$path":…}` directive, or `{{path}}` template).
        value: Json,
    },
    /// Toggle a boolean state path.
    Toggle {
        /// Target state path.
        path: String,
    },
    /// Switch the active view.
    Navigate {
        /// The [`UiView::id`] to show.
        view: String,
    },
    /// Select a tab within a [`NodeKind::Tabs`] container by child index.
    SelectTab {
        /// The `tabs` container node id.
        id: String,
        /// The zero-based index of the [`NodeKind::Tab`] child to activate.
        index: usize,
    },
    /// Open a dialog by node id.
    OpenDialog {
        /// The dialog node id.
        id: String,
    },
    /// Close a dialog by node id.
    CloseDialog {
        /// The dialog node id.
        id: String,
    },
    /// Append a value to an array state path. The value resolves like
    /// [`ClientOp::Set`]'s (`$path` directives and `{{path}}` references).
    Append {
        /// Target state path (an array).
        path: String,
        /// The value to append (literal, `{"$path":…}`, or `{{path}}` template).
        value: Json,
    },
    /// Remove the element at `index` from an array state path.
    RemoveAt {
        /// Target state path (an array).
        path: String,
        /// The index to remove.
        index: usize,
    },
    /// Start (or resume) a [`NodeKind::Timer`]/[`NodeKind::Stopwatch`] by node id.
    StartTimer {
        /// The timer node id.
        id: String,
    },
    /// Pause a running timer/stopwatch by node id.
    PauseTimer {
        /// The timer node id.
        id: String,
    },
    /// Reset a timer/stopwatch to its initial value (stopped) by node id.
    ResetTimer {
        /// The timer node id.
        id: String,
    },
}

/// A server→client action returned from a handler run (`/uis/{id}/event` or a
/// Boa script). Mirrors [`ClientOp`] plus a [`UiAction::Toast`]. **Append-only**.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum UiAction {
    /// Set a state path.
    Set {
        /// Target state path.
        path: String,
        /// The value.
        value: Json,
    },
    /// Switch the active view.
    Navigate {
        /// The [`UiView::id`] to show.
        view: String,
    },
    /// Select a tab within a [`NodeKind::Tabs`] container by child index.
    SelectTab {
        /// The `tabs` container node id.
        id: String,
        /// The zero-based index of the [`NodeKind::Tab`] child to activate.
        index: usize,
    },
    /// Open a dialog by node id.
    OpenDialog {
        /// The dialog node id.
        id: String,
    },
    /// Close a dialog by node id.
    CloseDialog {
        /// The dialog node id.
        id: String,
    },
    /// Show a transient toast/notification.
    Toast {
        /// One of `info` | `success` | `warn` | `error`.
        #[serde(default = "toast_info")]
        level: String,
        /// The message text.
        message: String,
    },
    /// Start (or resume) a timer/stopwatch by node id.
    StartTimer {
        /// The timer node id.
        id: String,
    },
    /// Pause a running timer/stopwatch by node id.
    PauseTimer {
        /// The timer node id.
        id: String,
    },
    /// Reset a timer/stopwatch to its initial value (stopped) by node id.
    ResetTimer {
        /// The timer node id.
        id: String,
    },
}

fn toast_info() -> String {
    "info".to_string()
}

// ---------------------------------------------------------------------------
// Validation rules
// ---------------------------------------------------------------------------

/// A single validation rule on an input node, with the user-facing `message`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ValidationRule {
    /// The rule.
    pub rule: ValidationKind,
    /// The message shown when the rule fails.
    pub message: String,
}

/// The (closed) set of validation kinds. Non-`Script` kinds evaluate
/// client-side; [`ValidationKind::Script`] defers to a server round-trip.
/// **Append-only**.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ValidationKind {
    /// The bound value must be present and non-empty.
    Required,
    /// Minimum string length.
    MinLen {
        /// The minimum length.
        n: usize,
    },
    /// Maximum string length.
    MaxLen {
        /// The maximum length.
        n: usize,
    },
    /// The value must match this regular expression.
    Pattern {
        /// The regex source.
        regex: String,
    },
    /// The numeric value must lie within `[min, max]`.
    Range {
        /// Inclusive lower bound.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min: Option<f64>,
        /// Inclusive upper bound.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<f64>,
    },
    /// A named Boa script returning `{ ok, message? }`.
    Script {
        /// A key into [`UiSpec::scripts`].
        handler: String,
    },
}

// ---------------------------------------------------------------------------
// Edit-by-patch (id-targeted; recommended over RFC-6902 positional pointers)
// ---------------------------------------------------------------------------

/// An atomic, id-targeted edit to a [`UiSpec`]. The large node/view payloads are
/// boxed to keep the enum small. **Append-only**.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum UiPatchOp {
    /// Replace (or, with `merge`, shallow-merge) a node's props.
    SetProps {
        /// Target node id.
        node_id: String,
        /// The props.
        props: Map,
        /// Merge into existing props instead of replacing.
        #[serde(default)]
        merge: bool,
    },
    /// Insert a new child under a parent node.
    InsertNode {
        /// The parent node id.
        parent_id: String,
        /// Insert position (default: append).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        index: Option<usize>,
        /// The new node.
        node: Box<UiNode>,
    },
    /// Remove a node (and its subtree) by id. A view root cannot be removed this
    /// way — use [`UiPatchOp::RemoveView`].
    RemoveNode {
        /// Target node id.
        node_id: String,
    },
    /// Move a node under a new parent.
    MoveNode {
        /// Target node id.
        node_id: String,
        /// The new parent node id.
        new_parent_id: String,
        /// Insert position (default: append).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        index: Option<usize>,
    },
    /// Replace a node (and its whole subtree) in place, keeping its position —
    /// the "rewrite one component" op. Unlike [`UiPatchOp::RemoveNode`], a view
    /// root IS a valid target (the view keeps a root either way). The
    /// replacement may carry a different id; global id uniqueness is re-checked
    /// by the caller's validation pass.
    ReplaceNode {
        /// Target node id.
        node_id: String,
        /// The replacement node.
        node: Box<UiNode>,
    },
    /// Set or clear a node's two-way `bind`.
    SetBind {
        /// Target node id.
        node_id: String,
        /// The binding path, or `null` to clear.
        bind: Option<String>,
    },
    /// Set or clear a node's `show_if`.
    SetShowIf {
        /// Target node id.
        node_id: String,
        /// The condition path, or `null` to clear.
        show_if: Option<String>,
    },
    /// Set or clear a node's `for_each`.
    SetForEach {
        /// Target node id.
        node_id: String,
        /// The loop binding, or `null` to clear.
        for_each: Option<ForEach>,
    },
    /// Set or clear a node's handler for one event.
    SetEvent {
        /// Target node id.
        node_id: String,
        /// The event.
        event: EventName,
        /// The handler, or `null` to clear.
        handler: Option<Handler>,
    },
    /// Replace a node's validation rules.
    SetValidate {
        /// Target node id.
        node_id: String,
        /// The new rules.
        rules: Vec<ValidationRule>,
    },
    /// Set or remove a named script.
    SetScript {
        /// The script name.
        name: String,
        /// The script, or `null` to remove.
        def: Option<ScriptDef>,
    },
    /// Set or remove a named computed value.
    SetComputed {
        /// The computed name.
        name: String,
        /// The definition, or `null` to remove.
        def: Option<ComputedDef>,
    },
    /// Add a new view.
    AddView {
        /// The new view.
        view: Box<UiView>,
    },
    /// Remove a view by id.
    RemoveView {
        /// The view id.
        view_id: String,
    },
    /// Replace a view's root node.
    SetViewRoot {
        /// The view id.
        view_id: String,
        /// The new root node.
        root: Box<UiNode>,
    },
    /// Update spec-level metadata.
    SetMeta {
        /// New default view.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default_view: Option<String>,
    },
    /// Replace or shallow-merge the App's seed state. This keeps staged App
    /// creation practical when later component subtrees bind to static data.
    SetInitialState {
        /// The seed-state object to install or merge.
        state: Map,
        /// Merge these top-level keys into the existing state instead of
        /// replacing the complete object.
        #[serde(default)]
        merge: bool,
    },
}

// ---------------------------------------------------------------------------
// Applying patches
// ---------------------------------------------------------------------------

/// Apply an ordered list of [`UiPatchOp`]s to `spec` in place — the "partial
/// edit an app" primitive (the id-targeted alternative to replacing the whole
/// [`UiSpec`] with `present_ui`).
///
/// This performs only the *structural* edit: it resolves each op's id target
/// and mutates the tree, returning [`UiSpecError::NodeNotFound`] /
/// [`UiSpecError::ViewNotFound`] / [`UiSpecError::BadNode`] when a target does
/// not resolve. It deliberately does **not** re-check the whole spec (unique
/// ids, container rules, reference integrity, size clamps): callers apply the
/// patch to a working clone and then run [`validate_ui_spec`] on the result, so
/// a rejected patch never reaches the store. Ops apply in order; the first
/// failure aborts the rest (the caller discards the half-mutated clone).
pub fn apply_ui_patch(spec: &mut UiSpec, ops: &[UiPatchOp]) -> Result<(), UiSpecError> {
    for op in ops {
        apply_patch_op(spec, op)?;
    }
    Ok(())
}

/// Apply a single [`UiPatchOp`]. See [`apply_ui_patch`].
fn apply_patch_op(spec: &mut UiSpec, op: &UiPatchOp) -> Result<(), UiSpecError> {
    match op {
        UiPatchOp::SetProps {
            node_id,
            props,
            merge,
        } => {
            let node = node_by_id_mut(spec, node_id)?;
            if *merge {
                for (k, v) in props.clone() {
                    node.props.insert(k, v);
                }
            } else {
                node.props = props.clone();
            }
        }
        UiPatchOp::InsertNode {
            parent_id,
            index,
            node,
        } => {
            let parent = node_by_id_mut(spec, parent_id)?;
            insert_child(parent, *index, (**node).clone());
        }
        UiPatchOp::RemoveNode { node_id } => {
            remove_node(spec, node_id)?;
        }
        UiPatchOp::MoveNode {
            node_id,
            new_parent_id,
            index,
        } => {
            // Detach first, then re-attach. If `new_parent_id` was inside the
            // moved subtree it is gone after the detach → `NodeNotFound`, which
            // is exactly the cycle guard we want.
            let node = remove_node(spec, node_id)?;
            let parent = node_by_id_mut(spec, new_parent_id)?;
            insert_child(parent, *index, node);
        }
        UiPatchOp::ReplaceNode { node_id, node } => {
            let target = node_by_id_mut(spec, node_id)?;
            *target = (**node).clone();
        }
        UiPatchOp::SetBind { node_id, bind } => {
            node_by_id_mut(spec, node_id)?.bind = bind.clone();
        }
        UiPatchOp::SetShowIf { node_id, show_if } => {
            node_by_id_mut(spec, node_id)?.show_if = show_if.clone();
        }
        UiPatchOp::SetForEach { node_id, for_each } => {
            node_by_id_mut(spec, node_id)?.for_each = for_each.clone();
        }
        UiPatchOp::SetEvent {
            node_id,
            event,
            handler,
        } => {
            let node = node_by_id_mut(spec, node_id)?;
            match handler {
                Some(h) => {
                    node.events.insert(*event, h.clone());
                }
                None => {
                    node.events.remove(event);
                }
            }
        }
        UiPatchOp::SetValidate { node_id, rules } => {
            node_by_id_mut(spec, node_id)?.validate = rules.clone();
        }
        UiPatchOp::SetScript { name, def } => match def {
            Some(d) => {
                spec.scripts.insert(name.clone(), d.clone());
            }
            None => {
                spec.scripts.remove(name);
            }
        },
        UiPatchOp::SetComputed { name, def } => {
            spec.computed.retain(|c| &c.name != name);
            if let Some(d) = def {
                spec.computed.push(d.clone());
            }
        }
        UiPatchOp::AddView { view } => {
            spec.views.push((**view).clone());
        }
        UiPatchOp::RemoveView { view_id } => {
            let before = spec.views.len();
            spec.views.retain(|v| &v.id != view_id);
            if spec.views.len() == before {
                return Err(UiSpecError::ViewNotFound(view_id.clone()));
            }
        }
        UiPatchOp::SetViewRoot { view_id, root } => {
            let view = spec
                .views
                .iter_mut()
                .find(|v| &v.id == view_id)
                .ok_or_else(|| UiSpecError::ViewNotFound(view_id.clone()))?;
            view.root = (**root).clone();
        }
        UiPatchOp::SetInitialState { state, merge } => {
            if *merge {
                for (key, value) in state.clone() {
                    spec.initial_state.insert(key, value);
                }
            } else {
                spec.initial_state = state.clone();
            }
        }
        UiPatchOp::SetMeta { default_view } => {
            if let Some(dv) = default_view {
                spec.default_view = dv.clone();
            }
        }
    }
    Ok(())
}

/// Mutable reference to the node with `id`, searched across every view root and
/// descendant. [`UiSpecError::NodeNotFound`] when absent.
fn node_by_id_mut<'a>(spec: &'a mut UiSpec, id: &str) -> Result<&'a mut UiNode, UiSpecError> {
    for v in &mut spec.views {
        if let Some(n) = node_find_mut(&mut v.root, id) {
            return Ok(n);
        }
    }
    Err(UiSpecError::NodeNotFound(id.to_string()))
}

fn node_find_mut<'a>(node: &'a mut UiNode, id: &str) -> Option<&'a mut UiNode> {
    if node.id == id {
        return Some(node);
    }
    for child in &mut node.children {
        if let Some(found) = node_find_mut(child, id) {
            return Some(found);
        }
    }
    None
}

/// Insert `child` under `parent` at `index` (clamped to `[0, len]`; `None`
/// appends).
fn insert_child(parent: &mut UiNode, index: Option<usize>, child: UiNode) {
    let len = parent.children.len();
    let at = index.unwrap_or(len).min(len);
    parent.children.insert(at, child);
}

/// Detach and return the node with `id`. A view **root** cannot be removed this
/// way ([`UiSpecError::BadNode`] — use [`UiPatchOp::RemoveView`]); an absent id
/// is [`UiSpecError::NodeNotFound`].
fn remove_node(spec: &mut UiSpec, id: &str) -> Result<UiNode, UiSpecError> {
    for v in &spec.views {
        if v.root.id == id {
            return Err(UiSpecError::BadNode {
                id: id.to_string(),
                kind: v.root.kind,
                reason: "cannot remove a view root; use the remove_view op".to_string(),
            });
        }
    }
    for v in &mut spec.views {
        if let Some(removed) = node_remove_child(&mut v.root, id) {
            return Ok(removed);
        }
    }
    Err(UiSpecError::NodeNotFound(id.to_string()))
}

fn node_remove_child(node: &mut UiNode, id: &str) -> Option<UiNode> {
    if let Some(pos) = node.children.iter().position(|c| c.id == id) {
        return Some(node.children.remove(pos));
    }
    for child in &mut node.children {
        if let Some(removed) = node_remove_child(child, id) {
            return Some(removed);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Limits enforced by [`validate_ui_spec`].
pub const MAX_DEPTH: usize = 32;
/// Maximum node count across all views.
pub const MAX_NODES: usize = 2000;
/// Maximum number of views.
pub const MAX_VIEWS: usize = 32;
/// Maximum `for_each` nesting depth. Nested loops multiply rendered rows; this
/// bounds the multiplication at authoring time (the wasm renderer also caps the
/// total rows produced via its `ROW_BUDGET`).
pub const MAX_FOR_EACH_DEPTH: usize = 4;
/// Maximum total array elements across `initial_state`. Bounds the seed a single
/// spec can ship (a `for_each` over a giant seeded array would otherwise hand the
/// renderer megabytes of rows before its row budget applies).
pub const MAX_INITIAL_STATE_ELEMENTS: usize = 10_000;

/// Why a [`UiSpec`] (or a patch result) was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UiSpecError {
    /// No views were defined.
    #[error("a ui spec must define at least one view")]
    NoViews,
    /// Two nodes (or views) share an id.
    #[error("duplicate id: {0}")]
    DuplicateId(String),
    /// A handler/computed/validation/navigate target does not resolve.
    #[error("unknown reference: {0}")]
    UnknownReference(String),
    /// A handler names a tool that is not registered.
    #[error("unknown tool: {0}")]
    UnknownTool(String),
    /// A handler names a tool that is not on the server allow-list.
    #[error("tool not allowed in ui handlers: {0}")]
    ToolNotAllowed(String),
    /// The tree exceeds [`MAX_DEPTH`].
    #[error("ui tree too deep (max {MAX_DEPTH})")]
    TooDeep,
    /// The spec exceeds a size bound.
    #[error("ui too large: {0}")]
    TooLarge(String),
    /// A node uses props/children/events invalid for its kind.
    #[error("invalid node `{id}` ({kind:?}): {reason}")]
    BadNode {
        /// The offending node id.
        id: String,
        /// Its kind.
        kind: NodeKind,
        /// Why it is invalid.
        reason: String,
    },
    /// A binding/condition path is not a plain dotted path.
    #[error("invalid binding expression: {0}")]
    BadExpr(String),
    /// A patch referenced a node that does not exist.
    #[error("node not found: {0}")]
    NodeNotFound(String),
    /// A patch referenced a view that does not exist.
    #[error("view not found: {0}")]
    ViewNotFound(String),
}

impl From<UiSpecError> for crate::error::Error {
    fn from(e: UiSpecError) -> Self {
        crate::error::Error::Invalid(e.to_string())
    }
}

/// Validate an AI-authored or AI-patched [`UiSpec`] before persisting it.
///
/// `tool_known` reports whether a tool name is registered; `tool_allowed`
/// reports whether it is on the server-defined UI-handler allow-list. Both are
/// closures so core stays decoupled from the API's tool registry/config.
pub fn validate_ui_spec<KN, AL>(
    spec: &UiSpec,
    tool_known: KN,
    tool_allowed: AL,
) -> Result<(), UiSpecError>
where
    KN: Fn(&str) -> bool,
    AL: Fn(&str) -> bool,
{
    if spec.views.is_empty() {
        return Err(UiSpecError::NoViews);
    }
    if spec.views.len() > MAX_VIEWS {
        return Err(UiSpecError::TooLarge(format!(
            "{} views (max {MAX_VIEWS})",
            spec.views.len()
        )));
    }

    // Bound the seed state: a giant `initial_state` array would feed the renderer
    // a huge `for_each` before its row budget kicks in.
    let elements: usize = spec.initial_state.values().map(count_json_elements).sum();
    if elements > MAX_INITIAL_STATE_ELEMENTS {
        return Err(UiSpecError::TooLarge(format!(
            "initial_state has {elements} array elements (max {MAX_INITIAL_STATE_ELEMENTS})"
        )));
    }

    // View ids: unique, and default_view resolves.
    let mut view_ids: Vec<&str> = Vec::with_capacity(spec.views.len());
    for v in &spec.views {
        if view_ids.contains(&v.id.as_str()) {
            return Err(UiSpecError::DuplicateId(v.id.clone()));
        }
        view_ids.push(&v.id);
    }
    if !view_ids.contains(&spec.default_view.as_str()) {
        return Err(UiSpecError::UnknownReference(format!(
            "default_view `{}`",
            spec.default_view
        )));
    }

    // Node ids unique across all views; collect + count + depth check.
    let mut node_ids: Vec<String> = Vec::new();
    for v in &spec.views {
        check_node(&v.root, 1, 0, &mut node_ids)?;
    }
    if node_ids.len() > MAX_NODES {
        return Err(UiSpecError::TooLarge(format!(
            "{} nodes (max {MAX_NODES})",
            node_ids.len()
        )));
    }

    // Reference integrity (handlers, computed, validation, navigate).
    for v in &spec.views {
        check_refs(&v.root, spec, &view_ids, &tool_known, &tool_allowed)?;
    }
    for c in &spec.computed {
        if !spec.scripts.contains_key(&c.handler) {
            return Err(UiSpecError::UnknownReference(format!(
                "computed `{}` → script `{}`",
                c.name, c.handler
            )));
        }
    }

    // `view_ref` composition must be acyclic — a view embedding itself (directly
    // or via a chain) would recurse forever in any renderer.
    check_view_ref_cycles(spec)?;

    // A sub-app's `parent_app` must at least be id-shaped; whether the parent
    // exists (and reciprocates with an `app_ref`) is verified server-side when
    // the shared namespace is actually resolved.
    if let Some(parent) = &spec.parent_app {
        if parent.trim().parse::<uuid::Uuid>().is_err() {
            return Err(UiSpecError::BadExpr(format!(
                "parent_app must be a ui id (UUID), got `{parent}`"
            )));
        }
    }
    Ok(())
}

/// Reject a `view_ref` cycle: build the view → referenced-views edges and DFS
/// for a back edge. Runs after per-node checks, so every `view_ref` already
/// names an existing view.
fn check_view_ref_cycles(spec: &UiSpec) -> Result<(), UiSpecError> {
    fn targets(node: &UiNode, out: &mut Vec<String>) {
        if node.kind == NodeKind::ViewRef {
            if let Some(v) = node.props.get("view").and_then(Json::as_str) {
                out.push(v.to_string());
            }
        }
        for c in &node.children {
            targets(c, out);
        }
    }
    let edges: BTreeMap<&str, Vec<String>> = spec
        .views
        .iter()
        .map(|v| {
            let mut out = Vec::new();
            targets(&v.root, &mut out);
            (v.id.as_str(), out)
        })
        .collect();

    // Iterative DFS with an explicit in-progress stack (specs are ≤ MAX_VIEWS
    // views, but recursion depth is unbounded by edges, not views — keep it flat).
    let mut done: Vec<&str> = Vec::new();
    for start in edges.keys() {
        if done.contains(start) {
            continue;
        }
        let mut stack: Vec<(&str, usize)> = vec![(start, 0)];
        let mut in_progress: Vec<&str> = vec![start];
        while let Some((view, next)) = stack.pop() {
            let out = edges.get(view).map(Vec::as_slice).unwrap_or(&[]);
            match out.get(next) {
                Some(target) => {
                    stack.push((view, next + 1));
                    let target = target.as_str();
                    if in_progress.contains(&target) {
                        return Err(UiSpecError::UnknownReference(format!(
                            "view_ref cycle through `{target}`"
                        )));
                    }
                    if !done.contains(&target) {
                        if let Some((known, _)) = edges.get_key_value(target) {
                            in_progress.push(known);
                            stack.push((known, 0));
                        }
                    }
                }
                None => {
                    in_progress.retain(|v| *v != view);
                    done.push(view);
                }
            }
        }
    }
    Ok(())
}

/// Every `app_ref` target ui id in the spec (the sub-apps a shell embeds), in
/// tree order, deduplicated. Used by the API to sanity-check a shell's targets
/// and to verify the shared-namespace parent chain.
#[must_use]
pub fn collect_app_refs(spec: &UiSpec) -> Vec<String> {
    fn walk(node: &UiNode, out: &mut Vec<String>) {
        if node.kind == NodeKind::AppRef {
            if let Some(app) = node.props.get("app").and_then(Json::as_str) {
                let app = app.trim().to_string();
                if !out.contains(&app) {
                    out.push(app);
                }
            }
        }
        for c in &node.children {
            walk(c, out);
        }
    }
    let mut out = Vec::new();
    for v in &spec.views {
        walk(&v.root, &mut out);
    }
    out
}

/// Whether `spec` embeds the App `app_id` via an `app_ref` node — the parent's
/// half of the mutual opt-in behind a shared `app_data` namespace.
#[must_use]
pub fn references_app(spec: &UiSpec, app_id: &str) -> bool {
    collect_app_refs(spec).iter().any(|a| a == app_id)
}

/// Total array elements within a JSON value, recursively — the metric the
/// `initial_state` size clamp ([`MAX_INITIAL_STATE_ELEMENTS`]) bounds.
fn count_json_elements(value: &Json) -> usize {
    match value {
        Json::Array(items) => items.len() + items.iter().map(count_json_elements).sum::<usize>(),
        // Count object *entries* too, not only array items — otherwise a key-heavy
        // object (no arrays) sums to 0 and slips past the seed-state bound.
        Json::Object(map) => map.len() + map.values().map(count_json_elements).sum::<usize>(),
        _ => 0,
    }
}

/// Recursively validate one node's shape (tree depth, `for_each` nesting depth,
/// unique id, per-kind structure, binding-path grammar) and collect its id.
fn check_node(
    node: &UiNode,
    depth: usize,
    loop_depth: usize,
    ids: &mut Vec<String>,
) -> Result<(), UiSpecError> {
    if depth > MAX_DEPTH {
        return Err(UiSpecError::TooDeep);
    }
    if node.id.is_empty() {
        return Err(UiSpecError::BadNode {
            id: node.id.clone(),
            kind: node.kind,
            reason: "empty node id".to_string(),
        });
    }
    if ids.contains(&node.id) {
        return Err(UiSpecError::DuplicateId(node.id.clone()));
    }
    ids.push(node.id.clone());

    if !node.children.is_empty() && !node.kind.is_container() {
        return Err(UiSpecError::BadNode {
            id: node.id.clone(),
            kind: node.kind,
            reason: "this kind cannot have children".to_string(),
        });
    }
    if matches!(node.kind, NodeKind::ConstrainedBox | NodeKind::AspectRatio)
        && node.children.len() > 1
    {
        return Err(UiSpecError::BadNode {
            id: node.id.clone(),
            kind: node.kind,
            reason: "this wrapper accepts at most one child".to_string(),
        });
    }
    if node.bind.is_some() && !node.kind.is_input() {
        return Err(UiSpecError::BadNode {
            id: node.id.clone(),
            kind: node.kind,
            reason: "only input kinds can `bind`".to_string(),
        });
    }
    for event in node.events.keys() {
        // `load` is a view lifecycle event. The renderer only fires it on the
        // view root, so accepting it lower in the tree would persist a handler
        // that can never run. `complete` is the timer's terminal event;
        // everything else needs an interactive kind.
        let allowed = match event {
            EventName::Load => {
                depth == 1 && (node.kind.is_container() || node.kind.is_interactive())
            }
            EventName::Complete => node.kind == NodeKind::Timer,
            _ => node.kind.is_interactive(),
        };
        if !allowed {
            return Err(UiSpecError::BadNode {
                id: node.id.clone(),
                kind: node.kind,
                reason: if *event == EventName::Load && depth != 1 {
                    "`load` only fires on a view root; move this handler to the view root node"
                        .to_string()
                } else {
                    format!("this kind cannot carry a `{event:?}` event")
                },
            });
        }
    }

    // Binding / condition path grammar.
    if let Some(b) = &node.bind {
        check_path(b)?;
    }
    if let Some(s) = &node.show_if {
        check_path(s.strip_prefix('!').unwrap_or(s))?;
    }
    if let Some(fe) = &node.for_each {
        check_path(&fe.source)?;
        for f in fe.filter.iter().chain(&fe.filters) {
            check_path(&f.query)?;
            if let Some(p) = &f.path {
                check_path(p)?;
            }
        }
    }

    // Kind-specific prop shapes that carry authority or cross-references.
    check_kind_props(node)?;

    // A `for_each` node repeats its subtree, so its children render one loop level
    // deeper; bound how deeply loops may nest.
    let child_loop_depth = if node.for_each.is_some() {
        let next = loop_depth + 1;
        if next > MAX_FOR_EACH_DEPTH {
            return Err(UiSpecError::TooLarge(format!(
                "for_each nested {next} deep (max {MAX_FOR_EACH_DEPTH})"
            )));
        }
        next
    } else {
        loop_depth
    };

    for c in &node.children {
        check_node(c, depth + 1, child_loop_depth, ids)?;
    }
    Ok(())
}

/// Validate the prop shapes of kinds whose props carry authority or a
/// cross-reference: an `image`'s `db` source, a `view_ref`'s target view name
/// (existence is checked in [`check_refs`]) and an `app_ref`'s target ui id.
fn check_kind_props(node: &UiNode) -> Result<(), UiSpecError> {
    let bad = |reason: String| UiSpecError::BadNode {
        id: node.id.clone(),
        kind: node.kind,
        reason,
    };
    match node.kind {
        NodeKind::ConstrainedBox => {
            for key in ["min_width", "max_width", "min_height", "max_height"] {
                if let Some(value) = node.props.get(key) {
                    let n = value
                        .as_f64()
                        .ok_or_else(|| bad(format!("`{key}` must be a number of pixels")))?;
                    if !(0.0..=10_000.0).contains(&n) {
                        return Err(bad(format!("`{key}` must be between 0 and 10000")));
                    }
                }
            }
            for (min_key, max_key) in [("min_width", "max_width"), ("min_height", "max_height")] {
                if let (Some(min), Some(max)) = (
                    node.props.get(min_key).and_then(Json::as_f64),
                    node.props.get(max_key).and_then(Json::as_f64),
                ) {
                    if min > max {
                        return Err(bad(format!("`{min_key}` cannot exceed `{max_key}`")));
                    }
                }
            }
            if let Some(align) = node.props.get("align") {
                if !matches!(align.as_str(), Some("start" | "center" | "end" | "stretch")) {
                    return Err(bad(
                        "`align` must be start, center, end, or stretch".to_string()
                    ));
                }
            }
            if let Some(overflow) = node.props.get("overflow") {
                if !matches!(overflow.as_str(), Some("visible" | "hidden" | "auto")) {
                    return Err(bad(
                        "`overflow` must be visible, hidden, or auto".to_string()
                    ));
                }
            }
        }
        NodeKind::AspectRatio => {
            if let Some(value) = node.props.get("ratio") {
                let ratio = value
                    .as_f64()
                    .ok_or_else(|| bad("`ratio` must be a number".to_string()))?;
                if !(0.05..=20.0).contains(&ratio) {
                    return Err(bad("`ratio` must be between 0.05 and 20".to_string()));
                }
            }
            if let Some(fit) = node.props.get("fit") {
                if !matches!(fit.as_str(), Some("contain" | "cover" | "fill")) {
                    return Err(bad("`fit` must be contain, cover, or fill".to_string()));
                }
            }
        }
        // `db`: { connection, sql, params?, column? } — the SQL lives in the
        // (server-held) spec, never in a client URL; params are bound values.
        NodeKind::Image => {
            if let Some(db) = node.props.get("db") {
                let obj = db
                    .as_object()
                    .ok_or_else(|| bad("`db` must be an object".to_string()))?;
                for key in ["connection", "sql"] {
                    if obj
                        .get(key)
                        .and_then(Json::as_str)
                        .is_none_or(|s| s.trim().is_empty())
                    {
                        return Err(bad(format!("`db.{key}` must be a non-empty string")));
                    }
                }
                if let Some(params) = obj.get("params") {
                    if !params.is_array() {
                        return Err(bad("`db.params` must be an array".to_string()));
                    }
                }
                if let Some(col) = obj.get("column") {
                    if !col.is_string() {
                        return Err(bad("`db.column` must be a string".to_string()));
                    }
                }
            }
        }
        NodeKind::ViewRef
            if node
                .props
                .get("view")
                .and_then(Json::as_str)
                .is_none_or(|s| s.trim().is_empty()) =>
        {
            return Err(bad("`view` (a view id) is required".to_string()));
        }
        NodeKind::AppRef => {
            let app = node
                .props
                .get("app")
                .and_then(Json::as_str)
                .map(str::trim)
                .ok_or_else(|| bad("`app` (a ui id or name) is required".to_string()))?;
            // A ui id (UUID) or a `present_ui` name slug — resolved at mount.
            if app.is_empty() || app.len() > 128 {
                return Err(bad(
                    "`app` must be a ui id or name (1–128 chars)".to_string()
                ));
            }
        }
        _ => {}
    }
    Ok(())
}

/// Recursively validate one node's references against the spec.
fn check_refs<KN, AL>(
    node: &UiNode,
    spec: &UiSpec,
    view_ids: &[&str],
    tool_known: &KN,
    tool_allowed: &AL,
) -> Result<(), UiSpecError>
where
    KN: Fn(&str) -> bool,
    AL: Fn(&str) -> bool,
{
    for handler in node.events.values() {
        check_handler_refs(handler, spec, view_ids, tool_known, tool_allowed)?;
    }
    // A `view_ref` must name an existing view (its cycle-freedom is checked
    // spec-wide in `validate_ui_spec`).
    if node.kind == NodeKind::ViewRef {
        if let Some(target) = node.props.get("view").and_then(Json::as_str) {
            if !view_ids.contains(&target) {
                return Err(UiSpecError::UnknownReference(format!(
                    "view_ref `{target}`"
                )));
            }
        }
    }
    for rule in &node.validate {
        if let ValidationKind::Script { handler } = &rule.rule {
            if !spec.scripts.contains_key(handler) {
                return Err(UiSpecError::UnknownReference(format!(
                    "validation script `{handler}`"
                )));
            }
        }
    }
    for c in &node.children {
        check_refs(c, spec, view_ids, tool_known, tool_allowed)?;
    }
    Ok(())
}

fn check_handler_refs<KN, AL>(
    handler: &Handler,
    spec: &UiSpec,
    view_ids: &[&str],
    tool_known: &KN,
    tool_allowed: &AL,
) -> Result<(), UiSpecError>
where
    KN: Fn(&str) -> bool,
    AL: Fn(&str) -> bool,
{
    match handler {
        Handler::Client { ops } => {
            for op in ops {
                if let ClientOp::Navigate { view } = op {
                    if !view_ids.contains(&view.as_str()) {
                        return Err(UiSpecError::UnknownReference(format!("navigate `{view}`")));
                    }
                }
            }
        }
        Handler::Ai { .. } => {}
        Handler::Tool { tool, then, .. } => {
            if !tool_known(tool) {
                return Err(UiSpecError::UnknownTool(tool.clone()));
            }
            if !tool_allowed(tool) {
                return Err(UiSpecError::ToolNotAllowed(tool.clone()));
            }
            for op in then {
                if let ClientOp::Navigate { view } = op {
                    if !view_ids.contains(&view.as_str()) {
                        return Err(UiSpecError::UnknownReference(format!("navigate `{view}`")));
                    }
                }
            }
        }
        Handler::Script { handler } => {
            if !spec.scripts.contains_key(handler) {
                return Err(UiSpecError::UnknownReference(format!("script `{handler}`")));
            }
        }
    }
    Ok(())
}

/// Reject anything that is not a plain dotted path (`a.b.c`, segments of
/// `[A-Za-z0-9_]`). This keeps a function-like expression language out of the
/// client interpreter — richer logic must be a Boa script.
fn check_path(path: &str) -> Result<(), UiSpecError> {
    if path.is_empty() {
        return Err(UiSpecError::BadExpr("empty path".to_string()));
    }
    for seg in path.split('.') {
        if seg.is_empty() || !seg.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(UiSpecError::BadExpr(path.to_string()));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Pure path helpers (also re-implemented in the wasm web crate, which has no
// core dep; kept here for server-side use + as the conformance reference).
// ---------------------------------------------------------------------------

/// Read a dotted path out of a JSON value. Missing → [`Json::Null`].
#[must_use]
pub fn get_path<'a>(root: &'a Json, path: &str) -> &'a Json {
    let mut cur = root;
    for seg in path.split('.') {
        match cur {
            Json::Object(map) => match map.get(seg) {
                Some(v) => cur = v,
                None => return &Json::Null,
            },
            Json::Array(arr) => match seg.parse::<usize>().ok().and_then(|i| arr.get(i)) {
                Some(v) => cur = v,
                None => return &Json::Null,
            },
            _ => return &Json::Null,
        }
    }
    cur
}

/// Write a value at a dotted path, creating intermediate objects as needed.
pub fn set_path(root: &mut Json, path: &str, value: Json) {
    if !root.is_object() {
        *root = Json::Object(serde_json::Map::new());
    }
    let mut cur = root;
    let mut segs = path.split('.').peekable();
    while let Some(seg) = segs.next() {
        let obj = match cur {
            Json::Object(map) => map,
            other => {
                *other = Json::Object(serde_json::Map::new());
                other.as_object_mut().expect("just set to object")
            }
        };
        if segs.peek().is_none() {
            obj.insert(seg.to_string(), value);
            return;
        }
        cur = obj
            .entry(seg.to_string())
            .or_insert_with(|| Json::Object(serde_json::Map::new()));
    }
}

/// JavaScript-like truthiness: `false`/`null`/`0`/`""`/`[]`/`{}`/absent are
/// falsy, everything else truthy.
#[must_use]
pub fn truthy(v: &Json) -> bool {
    match v {
        Json::Null => false,
        Json::Bool(b) => *b,
        Json::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Json::String(s) => !s.is_empty(),
        Json::Array(a) => !a.is_empty(),
        Json::Object(o) => !o.is_empty(),
    }
}

/// Render a JSON value as plain display text (strings verbatim, scalars
/// stringified, `null` → empty, containers → compact JSON).
#[must_use]
pub fn stringify(v: &Json) -> String {
    match v {
        Json::Null => String::new(),
        Json::String(s) => s.clone(),
        Json::Bool(b) => b.to_string(),
        Json::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec_one(root: UiNode) -> UiSpec {
        UiSpec {
            default_view: "main".to_string(),
            views: vec![UiView {
                id: "main".to_string(),
                title: "Main".to_string(),
                root,
            }],
            initial_state: Map::new(),
            computed: Vec::new(),
            scripts: BTreeMap::new(),
            parent_app: None,
        }
    }

    fn leaf(id: &str, kind: NodeKind) -> UiNode {
        UiNode {
            id: id.to_string(),
            kind,
            props: Map::new(),
            children: Vec::new(),
            bind: None,
            show_if: None,
            for_each: None,
            events: BTreeMap::new(),
            validate: Vec::new(),
        }
    }

    #[test]
    fn path_get_set_roundtrip() {
        let mut v = json!({});
        set_path(&mut v, "form.email", json!("a@b.c"));
        assert_eq!(get_path(&v, "form.email"), &json!("a@b.c"));
        assert_eq!(get_path(&v, "form.missing"), &Json::Null);
        set_path(&mut v, "form.email", json!("x"));
        assert_eq!(get_path(&v, "form.email"), &json!("x"));
    }

    #[test]
    fn truthy_rules() {
        assert!(!truthy(&json!(0)));
        assert!(!truthy(&json!("")));
        assert!(!truthy(&json!([])));
        assert!(!truthy(&json!({})));
        assert!(!truthy(&Json::Null));
        assert!(truthy(&json!(1)));
        assert!(truthy(&json!("x")));
        assert!(truthy(&json!([1])));
    }

    #[test]
    fn validates_happy_spec() {
        let mut root = leaf("box", NodeKind::Stack);
        root.children.push(leaf("name", NodeKind::TextInput));
        root.children[0].bind = Some("form.name".to_string());
        let spec = spec_one(root);
        assert!(validate_ui_spec(&spec, |_| true, |_| true).is_ok());
    }

    #[test]
    fn new_input_kinds_allow_bind_and_validate() {
        // The freshly-added input kinds must accept `bind` (and therefore validate).
        for kind in [
            NodeKind::NumberInput,
            NodeKind::DateInput,
            NodeKind::RadioGroup,
            NodeKind::Slider,
        ] {
            assert!(kind.is_input(), "{kind:?} should classify as an input");
            let mut field = leaf("f", kind);
            field.bind = Some("form.value".to_string());
            let spec = spec_one(input_in_stack(field));
            assert!(
                validate_ui_spec(&spec, |_| true, |_| true).is_ok(),
                "{kind:?} with a bind should validate"
            );
        }
    }

    #[test]
    fn tabs_hold_tab_children_and_select_tab_validates() {
        // A `tabs` container with two `tab` panels and a button whose client
        // handler selects the second tab — a fully valid spec.
        let mut tabs = leaf("tabs", NodeKind::Tabs);
        let mut t0 = leaf("t0", NodeKind::Tab);
        t0.children.push(leaf("t0body", NodeKind::Text));
        let mut t1 = leaf("t1", NodeKind::Tab);
        t1.children.push(leaf("t1body", NodeKind::Text));
        tabs.children = vec![t0, t1];

        let mut btn = leaf("go", NodeKind::Button);
        btn.events.insert(
            EventName::Click,
            Handler::Client {
                ops: vec![ClientOp::SelectTab {
                    id: "tabs".to_string(),
                    index: 1,
                }],
            },
        );

        let mut root = leaf("root", NodeKind::Stack);
        root.children = vec![tabs, btn];
        let spec = spec_one(root);
        assert!(NodeKind::Tabs.is_container() && NodeKind::Tab.is_container());
        assert!(validate_ui_spec(&spec, |_| true, |_| true).is_ok());
    }

    #[test]
    fn size_wrappers_accept_one_child_and_validate_bounds() {
        let mut constrained = leaf("limit", NodeKind::ConstrainedBox);
        constrained
            .props
            .insert("max_width".to_string(), json!(480));
        constrained
            .props
            .insert("max_height".to_string(), json!(320));
        constrained
            .props
            .insert("align".to_string(), json!("center"));
        constrained
            .props
            .insert("overflow".to_string(), json!("auto"));

        let mut ratio = leaf("ratio", NodeKind::AspectRatio);
        ratio.props.insert("ratio".to_string(), json!(16.0 / 9.0));
        ratio.props.insert("fit".to_string(), json!("cover"));
        ratio.children.push(leaf("image", NodeKind::Image));
        constrained.children.push(ratio);

        assert!(NodeKind::ConstrainedBox.is_container());
        assert!(NodeKind::AspectRatio.is_container());
        assert!(validate_ui_spec(&spec_one(constrained), |_| true, |_| true).is_ok());

        for props in [
            json!({ "min_width": 600, "max_width": 400 }),
            json!({ "max_height": "large" }),
            json!({ "align": "middle" }),
        ] {
            let mut bad = leaf("bad", NodeKind::ConstrainedBox);
            bad.props = serde_json::from_value(props).unwrap();
            assert!(matches!(
                validate_ui_spec(&spec_one(bad), |_| true, |_| true),
                Err(UiSpecError::BadNode { .. })
            ));
        }

        for props in [json!({ "ratio": 0 }), json!({ "fit": "crop" })] {
            let mut bad = leaf("bad", NodeKind::AspectRatio);
            bad.props = serde_json::from_value(props).unwrap();
            assert!(matches!(
                validate_ui_spec(&spec_one(bad), |_| true, |_| true),
                Err(UiSpecError::BadNode { .. })
            ));
        }

        let mut two = leaf("two", NodeKind::ConstrainedBox);
        two.children = vec![leaf("a", NodeKind::Text), leaf("b", NodeKind::Text)];
        assert!(matches!(
            validate_ui_spec(&spec_one(two), |_| true, |_| true),
            Err(UiSpecError::BadNode { .. })
        ));
    }

    #[test]
    fn display_kinds_reject_bind() {
        // The new content kinds are not inputs and must reject `bind`.
        for kind in [
            NodeKind::Image,
            NodeKind::Link,
            NodeKind::Badge,
            NodeKind::ProgressBar,
        ] {
            assert!(!kind.is_input(), "{kind:?} must not classify as an input");
            let mut node = leaf("d", kind);
            node.bind = Some("x".to_string());
            let spec = spec_one(input_in_stack(node));
            assert!(matches!(
                validate_ui_spec(&spec, |_| true, |_| true),
                Err(UiSpecError::BadNode { .. })
            ));
        }
    }

    #[test]
    fn chart_kinds_are_read_only_leaves() {
        // Every chart kind is a display leaf: not an input, not a container, not
        // interactive — so `bind`, `children` and `events` are all rejected, and a
        // bare chart validates.
        for kind in [
            NodeKind::PieChart,
            NodeKind::DonutChart,
            NodeKind::BarChart,
            NodeKind::LineChart,
            NodeKind::AreaChart,
            NodeKind::Sparkline,
            NodeKind::Gauge,
            NodeKind::RadarChart,
            NodeKind::Heatmap,
        ] {
            assert!(!kind.is_input(), "{kind:?} must not be an input");
            assert!(!kind.is_container(), "{kind:?} must not be a container");
            assert!(!kind.is_interactive(), "{kind:?} must not be interactive");

            // A bare chart (data lives in props) validates.
            assert!(
                validate_ui_spec(
                    &spec_one(input_in_stack(leaf("c", kind))),
                    |_| true,
                    |_| true
                )
                .is_ok(),
                "{kind:?} should validate as a bare leaf"
            );

            // Children are rejected.
            let mut with_kids = leaf("c", kind);
            with_kids.children.push(leaf("kid", NodeKind::Text));
            assert!(matches!(
                validate_ui_spec(&spec_one(input_in_stack(with_kids)), |_| true, |_| true),
                Err(UiSpecError::BadNode { .. })
            ));
        }
    }

    #[test]
    fn load_event_allowed_on_view_root_only() {
        // `load` on a (root) container validates — the lifecycle seam an App uses
        // to pull durable data on open.
        let mut root = leaf("root", NodeKind::Stack);
        root.events.insert(
            EventName::Load,
            Handler::Tool {
                tool: "app_data_list".to_string(),
                args: Map::new(),
                result_path: Some("stored".to_string()),
                then: Vec::new(),
            },
        );
        assert!(validate_ui_spec(&spec_one(root), |_| true, |_| true).is_ok());

        // A nested container used as a visual shell is not the view root. The
        // client never visits it as a lifecycle boundary, so accepting this
        // shape would silently leave database-backed collections empty.
        let mut shell = leaf("shell", NodeKind::Stack);
        shell.events.insert(
            EventName::Load,
            Handler::Tool {
                tool: "app_data_list".to_string(),
                args: Map::new(),
                result_path: Some("stored".to_string()),
                then: Vec::new(),
            },
        );
        let err = validate_ui_spec(&spec_one(input_in_stack(shell)), |_| true, |_| true)
            .expect_err("a nested load handler must be rejected");
        assert!(err.to_string().contains("`load` only fires on a view root"));

        // `load` on a non-container, non-interactive leaf is rejected…
        let mut txt = leaf("t", NodeKind::Text);
        txt.events
            .insert(EventName::Load, Handler::Client { ops: Vec::new() });
        assert!(matches!(
            validate_ui_spec(&spec_one(input_in_stack(txt)), |_| true, |_| true),
            Err(UiSpecError::BadNode { .. })
        ));

        // …and a *click* on a plain container is still rejected (the per-event
        // check must not widen the other events).
        let mut root = leaf("root", NodeKind::Stack);
        root.events
            .insert(EventName::Click, Handler::Client { ops: Vec::new() });
        assert!(matches!(
            validate_ui_spec(&spec_one(root), |_| true, |_| true),
            Err(UiSpecError::BadNode { .. })
        ));
    }

    #[test]
    fn for_each_filter_paths_validated() {
        let mk = |filter: ForEachFilter| {
            let mut row = leaf("row", NodeKind::Text);
            row.for_each = Some(ForEach {
                source: "recipes".to_string(),
                item: "r".to_string(),
                index: None,
                key: None,
                filter: Some(filter),
                filters: Vec::new(),
                paginate: None,
            });
            spec_one(input_in_stack(row))
        };

        // A well-formed filter validates.
        let ok = mk(ForEachFilter {
            path: Some("title".to_string()),
            query: "search".to_string(),
            mode: FilterMode::Contains,
        });
        assert!(validate_ui_spec(&ok, |_| true, |_| true).is_ok());

        // Its `query`/`path` obey the plain dotted-path grammar.
        for bad in [
            ForEachFilter {
                path: None,
                query: "a[0]".to_string(),
                mode: FilterMode::Contains,
            },
            ForEachFilter {
                path: Some("x || y".to_string()),
                query: "search".to_string(),
                mode: FilterMode::Equals,
            },
        ] {
            assert!(matches!(
                validate_ui_spec(&mk(bad), |_| true, |_| true),
                Err(UiSpecError::BadExpr(_))
            ));
        }
    }

    #[test]
    fn list_and_table_are_read_only_leaves() {
        for kind in [NodeKind::List, NodeKind::Table] {
            assert!(!kind.is_input(), "{kind:?} must not be an input");
            assert!(!kind.is_container(), "{kind:?} must not be a container");
            assert!(!kind.is_interactive(), "{kind:?} must not be interactive");

            // A bare collection leaf (data lives in props) validates.
            assert!(validate_ui_spec(
                &spec_one(input_in_stack(leaf("c", kind))),
                |_| true,
                |_| true
            )
            .is_ok());

            // Children are rejected.
            let mut with_kids = leaf("c", kind);
            with_kids.children.push(leaf("kid", NodeKind::Text));
            assert!(matches!(
                validate_ui_spec(&spec_one(input_in_stack(with_kids)), |_| true, |_| true),
                Err(UiSpecError::BadNode { .. })
            ));
        }
    }

    #[test]
    fn rejects_duplicate_ids() {
        let mut root = leaf("box", NodeKind::Stack);
        root.children.push(leaf("dup", NodeKind::Text));
        root.children.push(leaf("dup", NodeKind::Text));
        let spec = spec_one(root);
        assert_eq!(
            validate_ui_spec(&spec, |_| true, |_| true),
            Err(UiSpecError::DuplicateId("dup".to_string()))
        );
    }

    #[test]
    fn rejects_children_on_leaf() {
        let mut root = leaf("box", NodeKind::Stack);
        let mut bad = leaf("t", NodeKind::Text);
        bad.children.push(leaf("c", NodeKind::Text));
        root.children.push(bad);
        let spec = spec_one(root);
        assert!(matches!(
            validate_ui_spec(&spec, |_| true, |_| true),
            Err(UiSpecError::BadNode { .. })
        ));
    }

    #[test]
    fn rejects_unknown_tool_and_disallowed_tool() {
        let mut btn = leaf("go", NodeKind::Button);
        btn.events.insert(
            EventName::Click,
            Handler::Tool {
                tool: "delete_everything".to_string(),
                args: Map::new(),
                result_path: None,
                then: Vec::new(),
            },
        );
        let mut root = leaf("box", NodeKind::Stack);
        root.children.push(btn);
        let spec = spec_one(root);
        // Unknown tool.
        assert!(matches!(
            validate_ui_spec(&spec, |_| false, |_| true),
            Err(UiSpecError::UnknownTool(_))
        ));
        // Known but not allow-listed.
        assert!(matches!(
            validate_ui_spec(&spec, |_| true, |_| false),
            Err(UiSpecError::ToolNotAllowed(_))
        ));
    }

    #[test]
    fn rejects_function_like_paths() {
        let mut input = leaf("x", NodeKind::TextInput);
        input.bind = Some("form.value()".to_string());
        let spec = spec_one(input_in_stack(input));
        assert!(matches!(
            validate_ui_spec(&spec, |_| true, |_| true),
            Err(UiSpecError::BadExpr(_))
        ));
    }

    fn input_in_stack(input: UiNode) -> UiNode {
        let mut root = leaf("box", NodeKind::Stack);
        root.children.push(input);
        root
    }

    /// A `for_each` Stack wrapping `child` (one loop level).
    fn loop_stack(id: &str, child: UiNode) -> UiNode {
        let mut n = leaf(id, NodeKind::Stack);
        n.for_each = Some(ForEach {
            source: "items".to_string(),
            item: "item".to_string(),
            index: None,
            key: None,
            filter: None,
            filters: Vec::new(),
            paginate: None,
        });
        n.children = vec![child];
        n
    }

    #[test]
    fn rejects_oversized_initial_state() {
        let mut spec = spec_one(leaf("t", NodeKind::Text));
        spec.initial_state.insert(
            "big".to_string(),
            json!(vec![0_u32; MAX_INITIAL_STATE_ELEMENTS + 1]),
        );
        assert!(matches!(
            validate_ui_spec(&spec, |_| true, |_| true),
            Err(UiSpecError::TooLarge(_))
        ));
    }

    #[test]
    fn rejects_oversized_object_initial_state() {
        // A key-heavy object (no arrays at all) must also hit the seed-state cap —
        // it would previously sum to 0 elements and slip through.
        let mut spec = spec_one(leaf("t", NodeKind::Text));
        let obj: serde_json::Map<String, Json> = (0..=MAX_INITIAL_STATE_ELEMENTS)
            .map(|i| (format!("k{i}"), json!(i)))
            .collect();
        spec.initial_state
            .insert("big".to_string(), Json::Object(obj));
        assert!(matches!(
            validate_ui_spec(&spec, |_| true, |_| true),
            Err(UiSpecError::TooLarge(_))
        ));
    }

    #[test]
    fn rejects_deeply_nested_for_each() {
        // One loop level past the limit.
        let mut node = leaf("inner", NodeKind::Text);
        for i in 0..=MAX_FOR_EACH_DEPTH {
            node = loop_stack(&format!("l{i}"), node);
        }
        let spec = spec_one(node);
        assert!(matches!(
            validate_ui_spec(&spec, |_| true, |_| true),
            Err(UiSpecError::TooLarge(_))
        ));
    }

    #[test]
    fn allows_for_each_at_the_limit() {
        let mut node = leaf("inner", NodeKind::Text);
        for i in 0..MAX_FOR_EACH_DEPTH {
            node = loop_stack(&format!("l{i}"), node);
        }
        let spec = spec_one(node);
        assert!(validate_ui_spec(&spec, |_| true, |_| true).is_ok());
    }

    #[test]
    fn timers_carry_complete_and_ops_roundtrip() {
        // A `timer` may carry a `complete` handler…
        let mut timer = leaf("t", NodeKind::Timer);
        timer.props.insert("duration".to_string(), json!(600));
        timer.events.insert(
            EventName::Complete,
            Handler::Client {
                ops: vec![ClientOp::OpenDialog { id: "done".into() }],
            },
        );
        // …and a button can address it via the timer ops.
        let mut dialog = leaf("done", NodeKind::Dialog);
        dialog.children.push(leaf("msg", NodeKind::Text));
        let mut btn = leaf("go", NodeKind::Button);
        btn.events.insert(
            EventName::Click,
            Handler::Client {
                ops: vec![
                    ClientOp::ResetTimer { id: "t".into() },
                    ClientOp::StartTimer { id: "t".into() },
                ],
            },
        );
        let mut root = leaf("root", NodeKind::Stack);
        root.children = vec![timer, dialog, btn];
        let spec = spec_one(root);
        assert!(validate_ui_spec(&spec, |_| true, |_| true).is_ok());
        let s = serde_json::to_string(&spec).unwrap();
        let back: UiSpec = serde_json::from_str(&s).unwrap();
        assert_eq!(spec, back);

        // `complete` on a stopwatch (or anything that is not a `timer`) is rejected.
        let mut sw = leaf("s", NodeKind::Stopwatch);
        sw.events
            .insert(EventName::Complete, Handler::Client { ops: Vec::new() });
        assert!(matches!(
            validate_ui_spec(&spec_one(input_in_stack(sw)), |_| true, |_| true),
            Err(UiSpecError::BadNode { .. })
        ));
    }

    #[test]
    fn multi_filters_validate_all_paths() {
        let mut row = leaf("row", NodeKind::Text);
        row.for_each = Some(ForEach {
            source: "recipes".to_string(),
            item: "r".to_string(),
            index: None,
            key: None,
            filter: Some(ForEachFilter {
                path: Some("title".to_string()),
                query: "search".to_string(),
                mode: FilterMode::Contains,
            }),
            filters: vec![ForEachFilter {
                path: Some("category".to_string()),
                query: "cat".to_string(),
                mode: FilterMode::Equals,
            }],
            paginate: None,
        });
        assert!(
            validate_ui_spec(&spec_one(input_in_stack(row.clone())), |_| true, |_| true).is_ok()
        );

        // A bad path in the extra `filters` list is rejected too.
        row.for_each.as_mut().unwrap().filters[0].query = "a[0]".to_string();
        assert!(matches!(
            validate_ui_spec(&spec_one(input_in_stack(row)), |_| true, |_| true),
            Err(UiSpecError::BadExpr(_))
        ));
    }

    #[test]
    fn pagination_decodes_with_defaults() {
        // Bare `{}` → paged, default page size; `mode` round-trips snake_case.
        let bare: Pagination = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(bare.page_size, 20);
        assert_eq!(bare.mode, PageMode::Paged);

        let infinite: Pagination =
            serde_json::from_value(serde_json::json!({ "page_size": 5, "mode": "infinite" }))
                .unwrap();
        assert_eq!(infinite.page_size, 5);
        assert_eq!(infinite.mode, PageMode::Infinite);

        // A `for_each` carrying `paginate` validates like any other loop.
        let mut row = leaf("row", NodeKind::Text);
        row.for_each = Some(ForEach {
            source: "items".to_string(),
            item: "item".to_string(),
            index: None,
            key: None,
            filter: None,
            filters: Vec::new(),
            paginate: Some(bare),
        });
        assert!(validate_ui_spec(&spec_one(input_in_stack(row)), |_| true, |_| true).is_ok());
    }

    #[test]
    fn view_ref_must_resolve_and_be_acyclic() {
        let view = |id: &str, root: UiNode| UiView {
            id: id.to_string(),
            title: id.to_string(),
            root,
        };
        let view_ref = |id: &str, target: &str| {
            let mut n = leaf(id, NodeKind::ViewRef);
            n.props.insert("view".to_string(), json!(target));
            n
        };

        // A fragment embedded from another view validates.
        let mut main_root = leaf("main_root", NodeKind::Stack);
        main_root.children.push(view_ref("frag", "fragment"));
        let spec = UiSpec {
            default_view: "main".to_string(),
            views: vec![
                view("main", main_root.clone()),
                view("fragment", leaf("frag_root", NodeKind::Text)),
            ],
            initial_state: Map::new(),
            computed: Vec::new(),
            scripts: BTreeMap::new(),
            parent_app: None,
        };
        assert!(validate_ui_spec(&spec, |_| true, |_| true).is_ok());

        // An unknown target is rejected.
        let mut bad_root = leaf("main_root", NodeKind::Stack);
        bad_root.children.push(view_ref("frag", "missing"));
        let bad = UiSpec {
            views: vec![view("main", bad_root)],
            ..spec.clone()
        };
        assert!(matches!(
            validate_ui_spec(&bad, |_| true, |_| true),
            Err(UiSpecError::UnknownReference(_))
        ));

        // A cycle (A embeds B, B embeds A) is rejected.
        let mut a_root = leaf("a_root", NodeKind::Stack);
        a_root.children.push(view_ref("a_ref", "b"));
        let mut b_root = leaf("b_root", NodeKind::Stack);
        b_root.children.push(view_ref("b_ref", "a"));
        let cyclic = UiSpec {
            default_view: "a".to_string(),
            views: vec![view("a", a_root), view("b", b_root)],
            initial_state: Map::new(),
            computed: Vec::new(),
            scripts: BTreeMap::new(),
            parent_app: None,
        };
        assert!(matches!(
            validate_ui_spec(&cyclic, |_| true, |_| true),
            Err(UiSpecError::UnknownReference(_))
        ));
    }

    #[test]
    fn app_ref_takes_id_or_name_but_parent_app_requires_uuid() {
        let sub = "11111111-1111-1111-1111-111111111111";
        let mut app_ref = leaf("sub", NodeKind::AppRef);
        app_ref.props.insert("app".to_string(), json!(sub));
        let mut spec = spec_one(input_in_stack(app_ref));
        spec.parent_app = Some("22222222-2222-2222-2222-222222222222".to_string());
        assert!(validate_ui_spec(&spec, |_| true, |_| true).is_ok());
        assert_eq!(collect_app_refs(&spec), vec![sub.to_string()]);
        assert!(references_app(&spec, sub));
        assert!(!references_app(
            &spec,
            "33333333-3333-3333-3333-333333333333"
        ));

        // A name slug is a valid target too (resolved at mount)…
        let mut by_name = leaf("sub", NodeKind::AppRef);
        by_name.props.insert("app".to_string(), json!("recipes"));
        assert!(validate_ui_spec(&spec_one(input_in_stack(by_name)), |_| true, |_| true).is_ok());

        // …but an empty/overlong target is rejected at authoring.
        for bad in [json!(""), json!("  "), json!("x".repeat(129))] {
            let mut bad_ref = leaf("sub", NodeKind::AppRef);
            bad_ref.props.insert("app".to_string(), bad);
            assert!(matches!(
                validate_ui_spec(&spec_one(input_in_stack(bad_ref)), |_| true, |_| true),
                Err(UiSpecError::BadNode { .. })
            ));
        }
        // `parent_app` stays UUID-only (it is written once, precisely).
        let mut bad_parent = spec_one(leaf("t", NodeKind::Text));
        bad_parent.parent_app = Some("not-a-uuid".to_string());
        assert!(matches!(
            validate_ui_spec(&bad_parent, |_| true, |_| true),
            Err(UiSpecError::BadExpr(_))
        ));
    }

    #[test]
    fn image_db_source_shape_is_validated() {
        let mk = |db: Json| {
            let mut img = leaf("img", NodeKind::Image);
            img.props.insert("db".to_string(), db);
            spec_one(input_in_stack(img))
        };
        // Well-formed: connection + sql (+ optional params/column).
        assert!(validate_ui_spec(
            &mk(
                json!({ "connection": "shopdb", "sql": "SELECT photo FROM recipes WHERE id = $1",
                        "params": ["{{sel.id}}"], "column": "photo" })
            ),
            |_| true,
            |_| true
        )
        .is_ok());
        // Missing sql / non-object / bad params are all rejected.
        for bad in [
            json!({ "connection": "shopdb" }),
            json!("shopdb"),
            json!({ "connection": "shopdb", "sql": "SELECT 1", "params": "nope" }),
        ] {
            assert!(matches!(
                validate_ui_spec(&mk(bad), |_| true, |_| true),
                Err(UiSpecError::BadNode { .. })
            ));
        }
    }

    #[test]
    fn spec_json_roundtrips() {
        let mut root = leaf("box", NodeKind::Stack);
        root.children.push(leaf("hi", NodeKind::Text));
        let spec = spec_one(root);
        let s = serde_json::to_string(&spec).unwrap();
        let back: UiSpec = serde_json::from_str(&s).unwrap();
        assert_eq!(spec, back);
    }

    #[test]
    fn unknown_node_kind_is_rejected_at_deserialize() {
        let bad = json!({"id":"x","kind":"webview"});
        assert!(serde_json::from_value::<UiNode>(bad).is_err());
    }

    // -- apply_ui_patch (partial edits) --------------------------------------

    fn stack_with(children: Vec<UiNode>) -> UiSpec {
        let mut root = leaf("root", NodeKind::Stack);
        root.children = children;
        spec_one(root)
    }

    fn props(pairs: &[(&str, Json)]) -> Map {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn patch_set_props_merges_or_replaces() {
        let mut node = leaf("t", NodeKind::Text);
        node.props = props(&[("text", json!("hi")), ("keep", json!(1))]);
        let mut spec = stack_with(vec![node]);

        // merge:true keeps unlisted keys, overrides listed ones.
        apply_ui_patch(
            &mut spec,
            &[UiPatchOp::SetProps {
                node_id: "t".into(),
                props: props(&[("text", json!("bye"))]),
                merge: true,
            }],
        )
        .unwrap();
        let n = node_by_id_mut(&mut spec, "t").unwrap();
        assert_eq!(n.props.get("text"), Some(&json!("bye")));
        assert_eq!(n.props.get("keep"), Some(&json!(1)));

        // merge:false replaces the whole map.
        apply_ui_patch(
            &mut spec,
            &[UiPatchOp::SetProps {
                node_id: "t".into(),
                props: props(&[("text", json!("new"))]),
                merge: false,
            }],
        )
        .unwrap();
        let n = node_by_id_mut(&mut spec, "t").unwrap();
        assert_eq!(n.props.get("keep"), None);
        assert_eq!(n.props.get("text"), Some(&json!("new")));
    }

    #[test]
    fn patch_inserts_moves_and_removes_nodes() {
        let mut inner = leaf("inner", NodeKind::Stack);
        inner.children = vec![leaf("a", NodeKind::Text)];
        let mut spec = stack_with(vec![inner, leaf("b", NodeKind::Text)]);

        // Insert at an index under root.
        apply_ui_patch(
            &mut spec,
            &[UiPatchOp::InsertNode {
                parent_id: "root".into(),
                index: Some(0),
                node: Box::new(leaf("c", NodeKind::Text)),
            }],
        )
        .unwrap();
        let root = node_by_id_mut(&mut spec, "root").unwrap();
        assert_eq!(root.children[0].id, "c");

        // Move "a" out of "inner" and under root (append).
        apply_ui_patch(
            &mut spec,
            &[UiPatchOp::MoveNode {
                node_id: "a".into(),
                new_parent_id: "root".into(),
                index: None,
            }],
        )
        .unwrap();
        let inner = node_by_id_mut(&mut spec, "inner").unwrap();
        assert!(inner.children.is_empty());
        let root = node_by_id_mut(&mut spec, "root").unwrap();
        assert_eq!(root.children.last().unwrap().id, "a");

        // Remove a subtree.
        apply_ui_patch(
            &mut spec,
            &[UiPatchOp::RemoveNode {
                node_id: "b".into(),
            }],
        )
        .unwrap();
        assert!(node_by_id_mut(&mut spec, "b").is_err());
    }

    #[test]
    fn patch_replaces_a_subtree_in_place() {
        let mut inner = leaf("inner", NodeKind::Stack);
        inner.children = vec![leaf("a", NodeKind::Text)];
        let mut spec = stack_with(vec![inner, leaf("b", NodeKind::Text)]);

        // Replace "inner" (and its subtree) with a new fragment, keeping its
        // position before "b"; roundtrip through JSON to exercise the closed
        // op enum's deserialize path.
        let ops: Vec<UiPatchOp> = serde_json::from_value(json!([
            { "op": "replace_node", "node_id": "inner",
              "node": { "id": "inner", "kind": "card", "children": [
                  { "id": "c", "kind": "text", "props": { "text": "new" } }
              ] } }
        ]))
        .unwrap();
        apply_ui_patch(&mut spec, &ops).unwrap();
        let root = node_by_id_mut(&mut spec, "root").unwrap();
        assert_eq!(root.children[0].id, "inner");
        assert_eq!(root.children[0].kind, NodeKind::Card);
        assert_eq!(root.children[0].children[0].id, "c");
        assert!(
            node_by_id_mut(&mut spec, "a").is_err(),
            "old subtree is gone"
        );
        assert!(validate_ui_spec(&spec, |_| true, |_| true).is_ok());

        // Unlike remove_node, a view ROOT is a valid replace target.
        apply_ui_patch(
            &mut spec,
            &[UiPatchOp::ReplaceNode {
                node_id: "root".into(),
                node: Box::new(leaf("root", NodeKind::Stack)),
            }],
        )
        .unwrap();
        assert!(spec.views[0].root.children.is_empty());

        // Unknown target.
        assert!(matches!(
            apply_ui_patch(
                &mut spec,
                &[UiPatchOp::ReplaceNode {
                    node_id: "ghost".into(),
                    node: Box::new(leaf("x", NodeKind::Text)),
                }]
            ),
            Err(UiSpecError::NodeNotFound(_))
        ));
    }

    #[test]
    fn patch_rejects_bad_targets() {
        let mut spec = stack_with(vec![leaf("a", NodeKind::Text)]);

        // Unknown node id.
        assert!(matches!(
            apply_ui_patch(
                &mut spec,
                &[UiPatchOp::RemoveNode {
                    node_id: "nope".into()
                }]
            ),
            Err(UiSpecError::NodeNotFound(_))
        ));
        // A view root cannot be removed via remove_node.
        assert!(matches!(
            apply_ui_patch(
                &mut spec,
                &[UiPatchOp::RemoveNode {
                    node_id: "root".into()
                }]
            ),
            Err(UiSpecError::BadNode { .. })
        ));
        // Moving under a non-existent parent.
        assert!(matches!(
            apply_ui_patch(
                &mut spec,
                &[UiPatchOp::MoveNode {
                    node_id: "a".into(),
                    new_parent_id: "ghost".into(),
                    index: None,
                }]
            ),
            Err(UiSpecError::NodeNotFound(_))
        ));
        // Unknown view.
        assert!(matches!(
            apply_ui_patch(
                &mut spec,
                &[UiPatchOp::RemoveView {
                    view_id: "ghost".into()
                }]
            ),
            Err(UiSpecError::ViewNotFound(_))
        ));
    }

    #[test]
    fn patch_sets_and_clears_node_fields() {
        let mut input = leaf("email", NodeKind::TextInput);
        input.bind = Some("form.email".into());
        let mut spec = stack_with(vec![input]);

        // Clear the bind, then set an event and clear it again.
        apply_ui_patch(
            &mut spec,
            &[
                UiPatchOp::SetBind {
                    node_id: "email".into(),
                    bind: None,
                },
                UiPatchOp::SetEvent {
                    node_id: "email".into(),
                    event: EventName::Change,
                    handler: Some(Handler::Client { ops: Vec::new() }),
                },
            ],
        )
        .unwrap();
        let n = node_by_id_mut(&mut spec, "email").unwrap();
        assert!(n.bind.is_none());
        assert!(n.events.contains_key(&EventName::Change));

        apply_ui_patch(
            &mut spec,
            &[UiPatchOp::SetEvent {
                node_id: "email".into(),
                event: EventName::Change,
                handler: None,
            }],
        )
        .unwrap();
        let n = node_by_id_mut(&mut spec, "email").unwrap();
        assert!(n.events.is_empty());
    }

    #[test]
    fn patch_edits_spec_level_pieces() {
        let mut spec = stack_with(vec![leaf("a", NodeKind::Text)]);

        // Add a second view and repoint the default; roundtrip through JSON so the
        // deserialize path of the closed op enum is exercised too.
        let add: Vec<UiPatchOp> = serde_json::from_value(json!([
            { "op": "add_view", "view": { "id": "two", "title": "Two",
              "root": { "id": "root2", "kind": "stack" } } },
            { "op": "set_initial_state", "state": {
                "recipes": [{ "id": "pad-thai", "title": "Pad Thai" }]
            } },
            { "op": "set_initial_state", "state": { "search": "" }, "merge": true },
            { "op": "set_meta", "default_view": "two" }
        ]))
        .unwrap();
        apply_ui_patch(&mut spec, &add).unwrap();
        assert_eq!(spec.views.len(), 2);
        assert_eq!(spec.default_view, "two");
        assert_eq!(spec.initial_state["recipes"][0]["id"], "pad-thai");
        assert_eq!(spec.initial_state["search"], "");
        assert!(validate_ui_spec(&spec, |_| true, |_| true).is_ok());

        // Replace a view root, remove the other view, restore the default.
        apply_ui_patch(
            &mut spec,
            &[
                UiPatchOp::SetViewRoot {
                    view_id: "two".into(),
                    root: Box::new(leaf("root2", NodeKind::Stack)),
                },
                UiPatchOp::SetMeta {
                    default_view: Some("main".into()),
                },
                UiPatchOp::RemoveView {
                    view_id: "two".into(),
                },
            ],
        )
        .unwrap();
        assert_eq!(spec.views.len(), 1);
        assert!(validate_ui_spec(&spec, |_| true, |_| true).is_ok());
    }

    #[test]
    fn patched_result_is_still_validated() {
        // apply_ui_patch is purely structural — the caller re-validates. An insert
        // that duplicates an id applies cleanly but fails validation.
        let mut spec = stack_with(vec![leaf("a", NodeKind::Text)]);
        apply_ui_patch(
            &mut spec,
            &[UiPatchOp::InsertNode {
                parent_id: "root".into(),
                index: None,
                node: Box::new(leaf("a", NodeKind::Text)),
            }],
        )
        .unwrap();
        assert!(matches!(
            validate_ui_spec(&spec, |_| true, |_| true),
            Err(UiSpecError::DuplicateId(_))
        ));
    }
}
