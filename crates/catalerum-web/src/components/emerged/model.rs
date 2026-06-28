//! `Deserialize`-only wire mirrors of the core `catalerum-core::model_ui`
//! vocabulary (plus the [`UiDefinition`] envelope from `model.rs`).
//!
//! The wasm crate deliberately keeps no `catalerum-core` dependency, so — exactly
//! like every other type in [`crate::api`] — these re-declare the server's JSON
//! contract locally: all ids are plain `String`, timestamps are `String`
//! (RFC-3339), and tagged sum types reproduce the server's `tag`/`rename_all`.
//! Only `Deserialize` is derived (these are inbound). The closed enums add a
//! `#[serde(other)] Unknown` fallback so a future server-side variant degrades to
//! a neutral render instead of failing the whole decode.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value as Json;

/// A JSON object (kind-specific props, tool args, seed state).
pub type Map = serde_json::Map<String, Json>;

// ---------------------------------------------------------------------------
// Envelope (mirror of `catalerum-core::model::UiDefinition`)
// ---------------------------------------------------------------------------

/// One persisted emerged-UI row, as returned by `GET /uis/{id}` and embedded in
/// the `UiArtifact` WS frame.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct UiDefinition {
    /// Stable UI id (UUID string).
    pub id: String,
    /// Owning workspace.
    pub workspace_id: String,
    /// Who authored it.
    pub author: Author,
    /// Optional slug (Apps panel); absent in JSON when unset.
    #[serde(default)]
    pub name: Option<String>,
    /// Human title (artifact/app header).
    pub title: String,
    /// Optional description; absent in JSON when unset.
    #[serde(default)]
    pub description: Option<String>,
    /// JSONB format version (append-only enum evolution).
    #[serde(default)]
    pub spec_version: u32,
    /// Optimistic edit-concurrency counter.
    #[serde(default)]
    pub version: i64,
    /// The component tree.
    pub definition: UiSpec,
    /// RFC-3339 creation timestamp.
    #[serde(default)]
    pub created_at: String,
    /// RFC-3339 last-edit timestamp.
    #[serde(default)]
    pub updated_at: String,
}

impl UiDefinition {
    /// A display title for lists and the pin quick menu: the title, else the
    /// name slug, else a placeholder.
    #[must_use]
    pub fn display_title(&self) -> String {
        if !self.title.trim().is_empty() {
            self.title.clone()
        } else {
            self.name
                .clone()
                .unwrap_or_else(|| "Untitled app".to_string())
        }
    }
}

/// Who authored an object — a human or an agent (mirror of `model::Author`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Author {
    /// A human user.
    User {
        /// User id (UUID string).
        id: String,
    },
    /// An agent.
    Agent {
        /// Agent id (UUID string).
        id: String,
    },
}

// ---------------------------------------------------------------------------
// The spec
// ---------------------------------------------------------------------------

/// A complete emerged-UI definition: one or more views, seed state, derived
/// values, and named (server-side) scripts.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct UiSpec {
    /// The [`UiView::id`] shown on mount.
    pub default_view: String,
    /// The views of this mini-app (at least one).
    pub views: Vec<UiView>,
    /// Seeds the client-side transient state object on mount.
    #[serde(default)]
    pub initial_state: Map,
    /// Read-only derived values (each produced by a named [`ScriptDef`]). Not
    /// evaluated client-side in v1 (needs the Boa round-trip).
    #[serde(default)]
    pub computed: Vec<ComputedDef>,
    /// Named server-side scripts referenced by handlers/computed/validation.
    #[serde(default)]
    pub scripts: BTreeMap<String, ScriptDef>,
    /// For a sub-app of a shell suite: the shell's ui id. Sub-apps are hidden
    /// from the Apps panel list (they render inside their shell's `app_ref`).
    #[serde(default)]
    pub parent_app: Option<String>,
}

/// One view (screen) of a multi-view emerged UI.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct UiView {
    /// Stable, unique-within-the-spec view id ([`ClientOp::Navigate`] target).
    pub id: String,
    /// Human title.
    pub title: String,
    /// The root node rendered for this view.
    pub root: UiNode,
}

/// A read-only derived value, exposed to bindings at `computed.<name>`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ComputedDef {
    /// The name exposed under `computed.<name>`.
    pub name: String,
    /// A key into [`UiSpec::scripts`].
    pub handler: String,
}

/// A named server-side Boa script.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ScriptDef {
    /// The runtime; only `javascript` is supported (Boa).
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

/// A single node in the component tree. `id` is stable and unique within a spec.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct UiNode {
    /// Stable, unique-within-the-spec node id (patch target + `<For>` key).
    pub id: String,
    /// The (closed) node kind.
    pub kind: NodeKind,
    /// Bindable, kind-specific properties. String values may contain `{{path}}`.
    #[serde(default)]
    pub props: Map,
    /// Child nodes (container kinds only).
    #[serde(default)]
    pub children: Vec<UiNode>,
    /// Two-way value binding for input kinds: a state path like `form.email`.
    #[serde(default)]
    pub bind: Option<String>,
    /// Conditional render: a single truthy state path, optional leading `!`.
    #[serde(default)]
    pub show_if: Option<String>,
    /// Repeat this node once per element of a state array.
    #[serde(default)]
    pub for_each: Option<ForEach>,
    /// Event handlers keyed by [`EventName`].
    #[serde(default)]
    pub events: BTreeMap<EventName, Handler>,
    /// Validation rules (input kinds only).
    #[serde(default)]
    pub validate: Vec<ValidationRule>,
}

/// The closed vocabulary of node kinds (mirror of core `NodeKind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// Vertical container.
    Stack,
    /// Horizontal container.
    Row,
    /// Grid container.
    Grid,
    /// Bordered card container.
    Card,
    /// Overlay dialog (shown when opened).
    Dialog,
    /// A tab container: renders a header strip + the active `Tab` child's panel.
    Tabs,
    /// One tab panel inside a [`NodeKind::Tabs`] (`props.label` = header text).
    Tab,
    /// A single-child wrapper that applies min/max width and height bounds.
    ConstrainedBox,
    /// A single-child wrapper that preserves a width-to-height ratio.
    AspectRatio,
    /// Horizontal rule.
    Divider,
    /// Inline/paragraph text.
    Text,
    /// A heading (`props.level` 1–6).
    Heading,
    /// Markdown rendered through the XSS-safe renderer.
    Markdown,
    /// An image (`props.src`, `props.alt`; URL scheme-checked at render).
    Image,
    /// A hyperlink (`props.href`, `props.label`; URL scheme-checked at render).
    Link,
    /// A small status badge (`props.text`, `props.variant`).
    Badge,
    /// A determinate progress bar (`props.value`, `props.max`).
    ProgressBar,
    /// A pie chart (`props.data` = slices).
    PieChart,
    /// A donut chart (pie with a hole; `props.data` = slices).
    DonutChart,
    /// A bar chart (`props.data`; `props.horizontal` flips orientation).
    BarChart,
    /// A line chart (`props.data`).
    LineChart,
    /// An area (filled line) chart (`props.data`).
    AreaChart,
    /// A compact axis-less trend line (`props.data` = numbers).
    Sparkline,
    /// A radial gauge (`props.value`, `props.min`, `props.max`).
    Gauge,
    /// A radar/spider chart (`props.axes` + `props.data`).
    RadarChart,
    /// A heatmap grid (`props.data` = rows of numbers).
    Heatmap,
    /// A button.
    Button,
    /// A single-line text input.
    TextInput,
    /// A multi-line text input.
    Textarea,
    /// A numeric input (`<input type="number">`; binds a JSON number).
    NumberInput,
    /// A date input (`<input type="date">`; binds an ISO date string).
    DateInput,
    /// A `<select>` dropdown.
    Select,
    /// A radio-button group over `props.options` (binds the chosen value).
    RadioGroup,
    /// A checkbox.
    Checkbox,
    /// A range slider (`<input type="range">`; binds a JSON number).
    Slider,
    /// A read-only bullet/numbered list over a `data` array (`props.item` picks
    /// the display path within each element).
    List,
    /// A read-only column table over a `data` array of objects (`props.columns`).
    Table,
    /// A countdown timer (`props.duration` seconds; fires `complete` at zero).
    Timer,
    /// A count-up stopwatch.
    Stopwatch,
    /// Renders another view of this spec inline (`props.view` names it).
    ViewRef,
    /// Mounts another emerged UI inline (`props.app` = its ui id) — the shell seam.
    AppRef,
    /// An unknown/future kind (renders as a neutral container).
    #[serde(other)]
    Unknown,
}

impl NodeKind {
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
}

/// A loop binding: render the node once per element of a state array.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ForEach {
    /// State path to an array, e.g. `tasks`.
    #[serde(rename = "in")]
    pub source: String,
    /// The per-iteration variable name (default `item`).
    #[serde(rename = "as", default = "default_item")]
    pub item: String,
    /// Optional index variable name.
    #[serde(default)]
    pub index: Option<String>,
    /// Optional per-item key path (for stable `<For>` keys).
    #[serde(default)]
    pub key: Option<String>,
    /// Optional client-side row filter (live search / category pickers).
    #[serde(default)]
    pub filter: Option<ForEachFilter>,
    /// Additional row filters, ANDed with `filter` (search box + category
    /// dropdown at once).
    #[serde(default)]
    pub filters: Vec<ForEachFilter>,
    /// Optional client-side windowing over the filtered rows (mirror of core
    /// `Pagination`): numbered pages or grow-on-scroll. Absent = render all.
    #[serde(default)]
    pub paginate: Option<Pagination>,
}

fn default_item() -> String {
    "item".to_string()
}

/// Client-side row windowing over a [`ForEach`] (mirror of core `Pagination`):
/// only the current page/window reaches the DOM while the whole array stays in
/// state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub struct Pagination {
    /// Rows per page (`paged`) or per reveal increment (`infinite`); default 20.
    #[serde(default = "default_page_size")]
    pub page_size: usize,
    /// How the rows are windowed.
    #[serde(default)]
    pub mode: PageMode,
}

fn default_page_size() -> usize {
    20
}

/// How a [`Pagination`] reveals rows (mirror of core `PageMode`). An
/// unknown/future mode degrades to `paged`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PageMode {
    /// One fixed-size page at a time with prev/next controls.
    #[default]
    Paged,
    /// Grow the window as the bottom sentinel scrolls into view.
    Infinite,
    /// An unknown/future mode (falls back to `paged`).
    #[serde(other)]
    Unknown,
}

/// A declarative per-row filter on a [`ForEach`] (mirror of core
/// `ForEachFilter`), evaluated client-side against live state so a bound search
/// input narrows rows on every keystroke. A falsy query passes every row.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ForEachFilter {
    /// Path within each item to match against; unset = the item itself.
    #[serde(default)]
    pub path: Option<String>,
    /// State path holding the query value (bind an input to it).
    pub query: String,
    /// How the row value must match the query.
    #[serde(default)]
    pub mode: FilterMode,
}

/// How a [`ForEachFilter`] row value matches its query (mirror of core
/// `FilterMode`). An unknown/future mode degrades to no filtering.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterMode {
    /// Case-insensitive substring match on the stringified values.
    #[default]
    Contains,
    /// Exact JSON equality, with scalar-vs-string coercion.
    Equals,
    /// An unknown/future mode (rows pass unfiltered).
    #[serde(other)]
    Unknown,
}

/// The (closed) set of UI events (mirror of core `EventName`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventName {
    /// A click/activation.
    Click,
    /// A form submit.
    Submit,
    /// A committed value change.
    Change,
    /// An in-progress input.
    Input,
    /// A selection.
    Select,
    /// A dialog open.
    Open,
    /// A dialog close.
    Close,
    /// The view holding this node became active (fired client-side on mount +
    /// navigate-to, for a view's root node only).
    Load,
    /// A running countdown `timer` reached zero (fired client-side, once).
    Complete,
    /// An unknown/future event.
    #[serde(other)]
    Unknown,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// What an event does (mirror of core `Handler`).
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Handler {
    /// Pure client-side ops (dialogs, view switch, local state). No round-trip.
    Client {
        /// The ops applied locally, in order.
        #[serde(default)]
        ops: Vec<ClientOp>,
    },
    /// Call back to the AI as a new chat turn (P3, not yet wired client-side).
    Ai {
        /// Optional extra instruction prepended to the synthesized turn.
        #[serde(default)]
        prompt: Option<String>,
        /// Whether to include the current transient state in the turn.
        #[serde(default = "default_true")]
        include_state: bool,
    },
    /// Invoke a registry tool (P3, needs `/uis/{id}/event`).
    Tool {
        /// The registered tool name.
        tool: String,
        /// Tool arguments; string values may contain `{{path}}`.
        #[serde(default)]
        args: Map,
        /// Optional state path to write the tool result into.
        #[serde(default)]
        result_path: Option<String>,
        /// Client ops applied after a successful call.
        #[serde(default)]
        then: Vec<ClientOp>,
    },
    /// Run a named Boa script (P4, server-side, needs the host bridge).
    Script {
        /// A key into [`UiSpec::scripts`].
        handler: String,
    },
}

fn default_true() -> bool {
    true
}

/// A client-applied state mutation (mirror of core `ClientOp`).
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ClientOp {
    /// Set a state path to a literal value. A value of `{"$path":"a.b"}` copies
    /// from another state path.
    Set {
        /// Target state path.
        path: String,
        /// The value (or a `{"$path":…}` copy directive).
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
    /// Select a tab within a `tabs` container by child index.
    SelectTab {
        /// The `tabs` container node id.
        id: String,
        /// The zero-based index of the `tab` child to activate.
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
    /// Append a value to an array state path.
    Append {
        /// Target state path (an array).
        path: String,
        /// The value to append.
        value: Json,
    },
    /// Remove the element at `index` from an array state path.
    RemoveAt {
        /// Target state path (an array).
        path: String,
        /// The index to remove.
        index: usize,
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
    /// Reset a timer/stopwatch (stopped, zero) by node id.
    ResetTimer {
        /// The timer node id.
        id: String,
    },
}

/// A server→client action returned from `POST /uis/{id}/event` (mirror of core
/// `UiAction`), applied verbatim by [`UiState::apply_action`](super::state::UiState::apply_action).
/// Inbound only; the closed set the server can ask the client to do.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum UiAction {
    /// Set an (absolute) state path to a value.
    Set {
        /// Target state path.
        path: String,
        /// The value.
        value: Json,
    },
    /// Switch the active view.
    Navigate {
        /// The view id to show.
        view: String,
    },
    /// Select a tab within a `tabs` container by child index.
    SelectTab {
        /// The `tabs` container node id.
        id: String,
        /// The zero-based index of the `tab` child to activate.
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
    /// Show a transient inline notice.
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
    /// Reset a timer/stopwatch (stopped, zero) by node id.
    ResetTimer {
        /// The timer node id.
        id: String,
    },
}

fn toast_info() -> String {
    "info".to_string()
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// A single validation rule on an input node, with its user-facing `message`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ValidationRule {
    /// The rule.
    pub rule: ValidationKind,
    /// The message shown when the rule fails.
    pub message: String,
}

/// The (closed) set of validation kinds (mirror of core `ValidationKind`).
/// Non-`Script` kinds evaluate client-side; `Script` defers to the server (P4).
#[derive(Clone, Debug, PartialEq, Deserialize)]
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
    /// The value must match this regular expression (deferred in v1; see
    /// [`crate::components::emerged::state`]).
    Pattern {
        /// The regex source.
        regex: String,
    },
    /// The numeric value must lie within `[min, max]`.
    Range {
        /// Inclusive lower bound.
        #[serde(default)]
        min: Option<f64>,
        /// Inclusive upper bound.
        #[serde(default)]
        max: Option<f64>,
    },
    /// A named Boa script returning `{ ok, message? }` (server-side, P4).
    Script {
        /// A key into [`UiSpec::scripts`].
        handler: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decodes_full_definition_envelope() {
        // Mirrors the `GET /uis/{id}` / `UiArtifact` JSON shape exactly.
        let def: UiDefinition = serde_json::from_value(json!({
            "id": "11111111-1111-1111-1111-111111111111",
            "workspace_id": "22222222-2222-2222-2222-222222222222",
            "author": { "kind": "agent", "id": "33333333-3333-3333-3333-333333333333" },
            "title": "Contact",
            "spec_version": 1,
            "version": 4,
            "definition": {
                "default_view": "main",
                "views": [{
                    "id": "main", "title": "Contact",
                    "root": { "id": "root", "kind": "stack", "children": [
                        { "id": "lbl", "kind": "text", "props": { "text": "Hi {{form.name}}" } },
                        { "id": "name", "kind": "text_input", "bind": "form.name" },
                        { "id": "go", "kind": "button", "props": { "label": "Save" },
                          "events": { "click": { "kind": "client",
                            "ops": [{ "op": "navigate", "view": "main" }] } } }
                    ] }
                }]
            },
            "created_at": "2026-06-25T00:00:00Z",
            "updated_at": "2026-06-25T00:00:00Z"
        }))
        .expect("decodes");
        assert_eq!(def.title, "Contact");
        assert_eq!(def.version, 4);
        assert_eq!(def.definition.views[0].root.children.len(), 3);
        assert!(matches!(
            def.author,
            Author::Agent { ref id } if id.ends_with("333")
        ));
    }

    #[test]
    fn optional_fields_tolerate_absence() {
        // name/description/computed/scripts/initial_state all omitted.
        let spec: UiSpec = serde_json::from_value(json!({
            "default_view": "v",
            "views": [{ "id": "v", "title": "V", "root": { "id": "r", "kind": "text" } }]
        }))
        .expect("decodes minimal");
        assert!(spec.initial_state.is_empty());
        assert!(spec.scripts.is_empty());
        assert_eq!(spec.views[0].root.kind, NodeKind::Text);
    }

    #[test]
    fn unknown_kind_and_event_degrade_not_fail() {
        let node: UiNode = serde_json::from_value(json!({
            "id": "x", "kind": "webview",
            "events": { "hover": { "kind": "client" } }
        }))
        .expect("unknown kind/event must not fail the decode");
        assert_eq!(node.kind, NodeKind::Unknown);
        assert!(node.events.contains_key(&EventName::Unknown));
    }

    #[test]
    fn tabs_node_and_select_tab_op_decode() {
        let node: UiNode = serde_json::from_value(json!({
            "id": "t", "kind": "tabs", "children": [
                { "id": "a", "kind": "tab", "props": { "label": "First" } },
                { "id": "b", "kind": "tab", "props": { "label": "Second" } }
            ]
        }))
        .expect("tabs decodes");
        assert_eq!(node.kind, NodeKind::Tabs);
        assert_eq!(node.children[1].kind, NodeKind::Tab);

        let op: ClientOp = serde_json::from_value(json!({
            "op": "select_tab", "id": "t", "index": 1
        }))
        .expect("select_tab decodes");
        assert!(matches!(op, ClientOp::SelectTab { index: 1, .. }));
    }

    #[test]
    fn size_wrapper_kinds_decode() {
        let node: UiNode = serde_json::from_value(json!({
            "id": "limit", "kind": "constrained_box",
            "props": { "max_width": 480, "align": "center" },
            "children": [{
                "id": "ratio", "kind": "aspect_ratio",
                "props": { "ratio": 1.777, "fit": "cover" },
                "children": [{ "id": "photo", "kind": "image" }]
            }]
        }))
        .expect("size wrappers decode");
        assert_eq!(node.kind, NodeKind::ConstrainedBox);
        assert_eq!(node.children[0].kind, NodeKind::AspectRatio);
    }

    #[test]
    fn load_event_list_table_and_filter_decode() {
        // The recipe-app vocabulary: a root `load` handler, collection leaves,
        // and a filtered for_each.
        let node: UiNode = serde_json::from_value(json!({
            "id": "root", "kind": "stack",
            "events": { "load": { "kind": "tool", "tool": "app_data_list",
                                  "result_path": "stored" } },
            "children": [
                { "id": "tbl", "kind": "table",
                  "props": { "data": { "$path": "rows" }, "columns": ["title"] } },
                { "id": "ing", "kind": "list", "props": { "data": "{{sel.ingredients}}" } },
                { "id": "row", "kind": "text",
                  "for_each": { "in": "recipes", "as": "r",
                                "filter": { "query": "search", "path": "title" } } }
            ]
        }))
        .expect("decodes");
        assert!(node.events.contains_key(&EventName::Load));
        assert_eq!(node.children[0].kind, NodeKind::Table);
        assert_eq!(node.children[1].kind, NodeKind::List);
        let fe = node.children[2].for_each.as_ref().expect("for_each");
        let f = fe.filter.as_ref().expect("filter");
        assert_eq!(f.query, "search");
        assert_eq!(f.mode, FilterMode::Contains);

        // A future mode degrades to Unknown instead of failing the decode.
        let f: ForEachFilter =
            serde_json::from_value(json!({ "query": "q", "mode": "fuzzy" })).expect("decodes");
        assert_eq!(f.mode, FilterMode::Unknown);
    }

    #[test]
    fn timers_composition_and_multi_filters_decode() {
        // Timer with a `complete` handler + timer control ops.
        let node: UiNode = serde_json::from_value(json!({
            "id": "t", "kind": "timer",
            "props": { "duration": 600, "label": "Pasta", "auto_start": true },
            "events": { "complete": { "kind": "client",
                "ops": [{ "op": "open_dialog", "id": "done" }] } }
        }))
        .expect("timer decodes");
        assert_eq!(node.kind, NodeKind::Timer);
        assert!(node.events.contains_key(&EventName::Complete));
        for (op, json) in [
            ("start", json!({ "op": "start_timer", "id": "t" })),
            ("pause", json!({ "op": "pause_timer", "id": "t" })),
            ("reset", json!({ "op": "reset_timer", "id": "t" })),
        ] {
            let decoded: ClientOp = serde_json::from_value(json.clone()).expect(op);
            let action: UiAction = serde_json::from_value(json).expect(op);
            match (op, decoded, action) {
                ("start", ClientOp::StartTimer { .. }, UiAction::StartTimer { .. })
                | ("pause", ClientOp::PauseTimer { .. }, UiAction::PauseTimer { .. })
                | ("reset", ClientOp::ResetTimer { .. }, UiAction::ResetTimer { .. }) => {}
                (op, d, a) => panic!("{op} decoded wrong: {d:?} / {a:?}"),
            }
        }

        // Stopwatch, view_ref, app_ref kinds.
        for (kind, expect) in [
            ("stopwatch", NodeKind::Stopwatch),
            ("view_ref", NodeKind::ViewRef),
            ("app_ref", NodeKind::AppRef),
        ] {
            let n: UiNode = serde_json::from_value(json!({ "id": "x", "kind": kind })).expect(kind);
            assert_eq!(n.kind, expect);
        }

        // Multi-filter for_each + a sub-app's parent_app.
        let spec: UiSpec = serde_json::from_value(json!({
            "default_view": "v", "parent_app": "11111111-1111-1111-1111-111111111111",
            "views": [{ "id": "v", "title": "V", "root": {
                "id": "row", "kind": "text",
                "for_each": { "in": "recipes",
                    "filter": { "query": "search", "path": "title" },
                    "filters": [{ "query": "cat", "path": "category", "mode": "equals" }] }
            } }]
        }))
        .expect("spec decodes");
        assert_eq!(
            spec.parent_app.as_deref(),
            Some("11111111-1111-1111-1111-111111111111")
        );
        let fe = spec.views[0].root.for_each.as_ref().expect("for_each");
        assert!(fe.filter.is_some());
        assert_eq!(fe.filters.len(), 1);
        assert_eq!(fe.filters[0].mode, FilterMode::Equals);
    }

    #[test]
    fn pagination_decodes_and_falls_back() {
        // Explicit infinite paginate on a for_each.
        let fe: ForEach = serde_json::from_value(json!({
            "in": "rows", "paginate": { "page_size": 8, "mode": "infinite" }
        }))
        .expect("for_each with paginate decodes");
        let p = fe.paginate.expect("paginate present");
        assert_eq!(p.page_size, 8);
        assert_eq!(p.mode, PageMode::Infinite);

        // Bare `{}` → paged + default size; an unknown mode degrades to paged.
        let bare: Pagination = serde_json::from_value(json!({})).unwrap();
        assert_eq!(bare.page_size, 20);
        assert_eq!(bare.mode, PageMode::Paged);
        let future: Pagination = serde_json::from_value(json!({ "mode": "carousel" })).unwrap();
        assert_eq!(future.mode, PageMode::Unknown);
    }

    #[test]
    fn handler_and_clientop_tags_decode() {
        let h: Handler = serde_json::from_value(json!({
            "kind": "tool", "tool": "create_note",
            "args": { "title": "{{form.title}}" },
            "then": [{ "op": "set", "path": "saved", "value": true }]
        }))
        .expect("tool handler");
        match h {
            Handler::Tool { tool, then, .. } => {
                assert_eq!(tool, "create_note");
                assert_eq!(then.len(), 1);
            }
            _ => panic!("wrong handler"),
        }
    }
}
