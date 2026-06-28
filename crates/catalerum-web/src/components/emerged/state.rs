//! The client-only transient state of a mounted emerged UI — current view, open
//! dialogs, in-progress form values — plus the single `apply_op` reducer, the
//! two-way `bind` helpers, and client-side validation.
//!
//! Structure (the AI-authored spec) and transient state are separate signals:
//! the AI never touches state, the user never patches structure. State is never
//! persisted in v1 (server-affecting handlers ship a full client snapshot when
//! P3/P4 land).

use std::collections::BTreeMap;

use leptos::prelude::*;
use serde_json::Value as Json;

use super::model::{ClientOp, UiAction, UiNode, UiView, ValidationKind};
use super::path::{
    abs_data_path, get_path, interpolate, resolve_value, set_path, stringify, truthy, Scope,
};

/// The wall clock in milliseconds. On wasm this is `Date.now()`; native (unit
/// tests) it is a constant, keeping the pure [`TimerState`] math deterministic —
/// tests drive it with explicit `now` values instead.
#[must_use]
pub fn now_ms() -> f64 {
    #[cfg(target_arch = "wasm32")]
    {
        js_sys::Date::now()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        0.0
    }
}

/// The client-only state of one `timer`/`stopwatch` node: accumulated elapsed
/// time plus, while running, the instant the current run started. Pure math —
/// every transition takes an explicit `now` (milliseconds) so it unit-tests
/// without a clock.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TimerState {
    /// Elapsed milliseconds accumulated across completed runs.
    pub base_ms: f64,
    /// The `now` at which the current run started (`Some` ⇔ running).
    pub started_at_ms: Option<f64>,
}

impl TimerState {
    /// Whether the timer is currently running.
    #[must_use]
    pub fn running(&self) -> bool {
        self.started_at_ms.is_some()
    }

    /// Total elapsed milliseconds as of `now`.
    #[must_use]
    pub fn elapsed_ms(&self, now: f64) -> f64 {
        self.base_ms + self.started_at_ms.map_or(0.0, |s| (now - s).max(0.0))
    }

    /// Start (or resume) at `now`. A no-op while already running.
    pub fn start(&mut self, now: f64) {
        if self.started_at_ms.is_none() {
            self.started_at_ms = Some(now);
        }
    }

    /// Pause at `now`, folding the current run into `base_ms`.
    pub fn pause(&mut self, now: f64) {
        if let Some(s) = self.started_at_ms.take() {
            self.base_ms += (now - s).max(0.0);
        }
    }

    /// Stop and zero the timer.
    pub fn reset(&mut self) {
        *self = TimerState::default();
    }

    /// Stop with the elapsed time clamped to exactly `at_ms` — the countdown's
    /// terminal transition. Returns `true` only when this call actually stopped
    /// a running timer (the once-only guard for the `complete` event).
    pub fn finish(&mut self, at_ms: f64) -> bool {
        let was_running = self.started_at_ms.take().is_some();
        if was_running {
            self.base_ms = at_ms;
        }
        was_running
    }
}

/// The reactive, client-only state of one mounted UI. `Copy` (every field is a
/// signal handle) so it threads freely into the render tree's closures.
#[derive(Clone, Copy)]
pub struct UiState {
    /// The UI's id (UUID string) — the `POST /uis/{id}/event` target. A
    /// `StoredValue` keeps [`UiState`] `Copy` while carrying the owned id into the
    /// render tree's event closures.
    pub ui_id: StoredValue<String>,
    /// Sink for `ai` handlers: runs a synthesized message as a new chat turn.
    /// `None` outside a chat (e.g. the Apps panel) → `ai` handlers show a notice.
    pub ai_sink: Option<UnsyncCallback<String>>,
    /// The transient state object (form values, flags, tool results).
    pub data: RwSignal<Json>,
    /// The active view id.
    pub view: RwSignal<String>,
    /// Open dialog node ids.
    pub dialogs: RwSignal<Vec<String>>,
    /// Active tab index per `tabs` container node id (absent ⇒ first tab).
    pub tabs: RwSignal<BTreeMap<String, usize>>,
    /// Pagination cursor per `for_each` node id: the current page index in
    /// `paged` mode, or the number of revealed rows in `infinite` mode (absent
    /// ⇒ the first page / one page revealed).
    pub pages: RwSignal<BTreeMap<String, usize>>,
    /// Timer/stopwatch run state per node id (absent ⇒ stopped at zero).
    pub timers: RwSignal<BTreeMap<String, TimerState>>,
    /// A transient inline notice (e.g. a not-yet-wired handler), dismissable.
    pub notice: RwSignal<Option<String>>,
    /// Async server-side validation errors keyed by input node id (the
    /// [`ValidationKind::Script`] round-trip result; empty/absent = valid).
    pub script_errors: RwSignal<BTreeMap<String, String>>,
    /// This spec's views, for inline `view_ref` composition (populated on mount).
    pub views: StoredValue<Vec<UiView>>,
    /// The `app_ref` mount chain ending in this UI's id — the cycle/depth guard
    /// for nested shell apps (a child whose id is already in the chain, or a
    /// chain past [`MAX_APP_DEPTH`], renders a notice instead of recursing).
    pub app_chain: StoredValue<Vec<String>>,
    /// Whether the spec declares `computed.*` values (drives the debounced
    /// live recompute as bound inputs change).
    pub has_computed: bool,
    /// Debounce generation for the live `computed.*` refresh (see
    /// [`handlers::bind_changed`](super::handlers::bind_changed)).
    pub compute_gen: StoredValue<u64>,
    /// Per-node debounce generations for `input` event handlers (see
    /// [`handlers::dispatch_input_debounced`](super::handlers::dispatch_input_debounced)).
    pub input_gens: StoredValue<BTreeMap<String, u64>>,
}

/// Deepest `app_ref` nesting (a shell embedding sub-apps embedding …).
pub const MAX_APP_DEPTH: usize = 4;

impl UiState {
    /// Seed a fresh state for UI `ui_id` from the spec's `initial_state` and
    /// `default_view`, with `ai_sink` for `ai` handlers. The mount fills in
    /// `views`/`app_chain`/`has_computed` (spec-derived) afterwards.
    #[must_use]
    pub fn seed(
        ui_id: String,
        ai_sink: Option<UnsyncCallback<String>>,
        initial_state: Json,
        default_view: String,
    ) -> UiState {
        let data = if initial_state.is_object() {
            initial_state
        } else {
            Json::Object(serde_json::Map::new())
        };
        UiState {
            ui_id: StoredValue::new(ui_id),
            ai_sink,
            data: RwSignal::new(data),
            view: RwSignal::new(default_view),
            dialogs: RwSignal::new(Vec::new()),
            tabs: RwSignal::new(BTreeMap::new()),
            pages: RwSignal::new(BTreeMap::new()),
            timers: RwSignal::new(BTreeMap::new()),
            notice: RwSignal::new(None),
            script_errors: RwSignal::new(BTreeMap::new()),
            views: StoredValue::new(Vec::new()),
            app_chain: StoredValue::new(Vec::new()),
            has_computed: false,
            compute_gen: StoredValue::new(0),
            input_gens: StoredValue::new(BTreeMap::new()),
        }
    }

    // --- timers -------------------------------------------------------------

    /// The [`TimerState`] of a timer node (default: stopped at zero).
    #[must_use]
    pub fn timer(&self, node_id: &str) -> TimerState {
        self.timers
            .with(|m| m.get(node_id).copied().unwrap_or_default())
    }

    /// Whether a timer node is currently running (untracked read — for interval
    /// callbacks that must not subscribe).
    #[must_use]
    pub fn timer_running_untracked(&self, node_id: &str) -> bool {
        self.timers
            .with_untracked(|m| m.get(node_id).is_some_and(TimerState::running))
    }

    /// Start (or resume) a timer node now.
    pub fn start_timer(&self, node_id: &str) {
        let now = now_ms();
        self.timers
            .update(|m| m.entry(node_id.to_string()).or_default().start(now));
    }

    /// Pause a timer node now.
    pub fn pause_timer(&self, node_id: &str) {
        let now = now_ms();
        self.timers.update(|m| {
            if let Some(t) = m.get_mut(node_id) {
                t.pause(now);
            }
        });
    }

    /// Reset a timer node (stopped, zero).
    pub fn reset_timer(&self, node_id: &str) {
        self.timers.update(|m| {
            m.entry(node_id.to_string()).or_default().reset();
        });
    }

    /// Stop a countdown exactly at `at_ms` elapsed; `true` only for the call
    /// that actually stopped it (the `complete` once-only guard).
    pub fn finish_timer(&self, node_id: &str, at_ms: f64) -> bool {
        let mut fired = false;
        self.timers.update(|m| {
            if let Some(t) = m.get_mut(node_id) {
                fired = t.finish(at_ms);
            }
        });
        fired
    }

    /// The active tab index for a `tabs` container, clamped to `[0, count)`
    /// (absent ⇒ first tab). `count` is the number of `tab` children.
    #[must_use]
    pub fn active_tab(&self, node_id: &str, count: usize) -> usize {
        let raw = self.tabs.with(|m| m.get(node_id).copied().unwrap_or(0));
        raw.min(count.saturating_sub(1))
    }

    /// Activate tab `index` within a `tabs` container.
    pub fn set_tab(&self, node_id: &str, index: usize) {
        self.tabs.update(|m| {
            m.insert(node_id.to_string(), index);
        });
    }

    // --- pagination ---------------------------------------------------------

    /// The pagination cursor for a `for_each` node (page index in `paged` mode,
    /// revealed-row count in `infinite` mode), or `default` when unset. Tracked —
    /// reading it inside the loop's `<For>` re-windows the rows on page changes.
    #[must_use]
    pub fn page(&self, node_id: &str, default: usize) -> usize {
        self.pages
            .with(|m| m.get(node_id).copied().unwrap_or(default))
    }

    /// Like [`page`](Self::page) but untracked — for the infinite-scroll observer
    /// callback, which must read the current cursor without subscribing.
    #[must_use]
    pub fn page_untracked(&self, node_id: &str, default: usize) -> usize {
        self.pages
            .with_untracked(|m| m.get(node_id).copied().unwrap_or(default))
    }

    /// Set the pagination cursor for a `for_each` node.
    pub fn set_page(&self, node_id: &str, cursor: usize) {
        self.pages.update(|m| {
            m.insert(node_id.to_string(), cursor);
        });
    }

    /// The current value of a two-way `bind` path as raw JSON (for posting to a
    /// server-side validation script).
    #[must_use]
    pub fn bind_json(&self, scope: &Scope, bind: &str) -> Json {
        self.data.with(|d| resolve_value(scope, d, bind))
    }

    /// Set (or, with `None`, clear) the async validation error for an input node.
    pub fn set_script_error(&self, node_id: &str, message: Option<String>) {
        self.script_errors.update(|m| match message {
            Some(text) => {
                m.insert(node_id.to_string(), text);
            }
            None => {
                m.remove(node_id);
            }
        });
    }

    /// The async (server) validation error for an input node, if any.
    #[must_use]
    pub fn script_error(&self, node_id: &str) -> Option<String> {
        self.script_errors.with(|m| m.get(node_id).cloned())
    }

    /// Store the server-derived `computed.*` values (SOUL §12) under the `computed`
    /// state key, so bindings like `{{computed.total}}` resolve. Set on mount and
    /// refreshed by the `set computed` action a handler response carries.
    pub fn set_computed(&self, computed: Json) {
        self.data.update(|d| set_path(d, "computed", computed));
    }

    /// An untracked clone of the transient state — the snapshot posted to the
    /// server with an authority-bearing event (read outside any reactive scope,
    /// so it never subscribes the firing closure).
    #[must_use]
    pub fn snapshot(&self) -> Json {
        self.data.get_untracked()
    }

    /// Apply one server-returned [`UiAction`] (the `POST /uis/{id}/event`
    /// response). Mirrors [`apply_op`](Self::apply_op) but over the closed,
    /// already-resolved server vocabulary (absolute paths, no scope).
    pub fn apply_action(&self, action: &UiAction) {
        match action {
            UiAction::Set { path, value } => self.data.update(|d| set_path(d, path, value.clone())),
            UiAction::Navigate { view } => self.view.set(view.clone()),
            UiAction::SelectTab { id, index } => self.set_tab(id, *index),
            UiAction::OpenDialog { id } => self.dialogs.update(|d| {
                if !d.iter().any(|x| x == id) {
                    d.push(id.clone());
                }
            }),
            UiAction::CloseDialog { id } => self.dialogs.update(|d| d.retain(|x| x != id)),
            UiAction::StartTimer { id } => self.start_timer(id),
            UiAction::PauseTimer { id } => self.pause_timer(id),
            UiAction::ResetTimer { id } => self.reset_timer(id),
            UiAction::Toast { level, message } => {
                let text = if level == "info" {
                    message.clone()
                } else {
                    format!("{level}: {message}")
                };
                self.notice.set(Some(text));
            }
        }
    }

    /// Apply one client op, routing data mutations through [`apply_data_op`] and
    /// view/dialog ops to their own signals.
    pub fn apply_op(&self, scope: &Scope, op: &ClientOp) {
        match op {
            ClientOp::Navigate { view } => self.view.set(view.clone()),
            ClientOp::SelectTab { id, index } => self.set_tab(id, *index),
            ClientOp::OpenDialog { id } => self.dialogs.update(|d| {
                if !d.iter().any(|x| x == id) {
                    d.push(id.clone());
                }
            }),
            ClientOp::CloseDialog { id } => self.dialogs.update(|d| d.retain(|x| x != id)),
            ClientOp::StartTimer { id } => self.start_timer(id),
            ClientOp::PauseTimer { id } => self.pause_timer(id),
            ClientOp::ResetTimer { id } => self.reset_timer(id),
            data_op => self.data.update(|d| apply_data_op(d, scope, data_op)),
        }
    }

    /// The current string value of a two-way `bind` path.
    #[must_use]
    pub fn bind_string(&self, scope: &Scope, bind: &str) -> String {
        self.data
            .with(|d| stringify(&resolve_value(scope, d, bind)))
    }

    /// The current boolean value of a two-way `bind` path (for checkboxes).
    #[must_use]
    pub fn bind_bool(&self, scope: &Scope, bind: &str) -> bool {
        self.data.with(|d| truthy(&resolve_value(scope, d, bind)))
    }

    /// Write a value to a two-way `bind` path.
    pub fn set_bind(&self, scope: &Scope, bind: &str, value: Json) {
        if let Some(p) = abs_data_path(scope, bind) {
            self.data.update(|d| set_path(d, &p, value));
        }
    }

    /// Whether a dialog node is currently open.
    #[must_use]
    pub fn is_dialog_open(&self, id: &str) -> bool {
        self.dialogs.with(|d| d.iter().any(|x| x == id))
    }

    /// Evaluate `show_if` (a single truthy path, optional leading `!`).
    #[must_use]
    pub fn show_if(&self, scope: &Scope, cond: &str) -> bool {
        let (negate, path) = match cond.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, cond),
        };
        let t = self.data.with(|d| truthy(&resolve_value(scope, d, path)));
        t ^ negate
    }

    /// The first failing validation message for an input node, if any. `Pattern`
    /// and `Script` kinds are deferred to the server in v1 and never fail here.
    #[must_use]
    pub fn field_error(&self, scope: &Scope, node: &UiNode) -> Option<String> {
        let bind = node.bind.as_deref()?;
        let value = self.data.with(|d| resolve_value(scope, d, bind));
        node.validate
            .iter()
            .find(|r| !rule_passes(&r.rule, &value))
            .map(|r| r.message.clone())
    }
}

/// Whether a single validation rule passes for `value`. Deferred kinds pass.
fn rule_passes(kind: &ValidationKind, value: &Json) -> bool {
    match kind {
        ValidationKind::Required => truthy(value),
        ValidationKind::MinLen { n } => stringify(value).chars().count() >= *n,
        ValidationKind::MaxLen { n } => stringify(value).chars().count() <= *n,
        ValidationKind::Range { min, max } => {
            let Some(f) = number_of(value) else {
                return true;
            };
            min.is_none_or(|lo| f >= lo) && max.is_none_or(|hi| f <= hi)
        }
        // `Pattern` needs a regex engine (no crate dep; browser `RegExp` aborts on
        // a malformed AI-authored pattern) and `Script` needs the Boa round-trip —
        // both are enforced server-side once P4 lands. Advisory-pass client-side.
        ValidationKind::Pattern { .. } | ValidationKind::Script { .. } => true,
    }
}

fn number_of(value: &Json) -> Option<f64> {
    match value {
        Json::Number(n) => n.as_f64(),
        Json::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// Apply a *data* client op to the state object in place. Pure (no signals) so it
/// is unit-testable natively; view/dialog ops are handled by [`UiState::apply_op`].
pub fn apply_data_op(data: &mut Json, scope: &Scope, op: &ClientOp) {
    match op {
        ClientOp::Set { path, value } => {
            let Some(target) = abs_data_path(scope, path) else {
                return;
            };
            let resolved = resolve_set_value(data, scope, value);
            set_path(data, &target, resolved);
        }
        ClientOp::Toggle { path } => {
            let Some(target) = abs_data_path(scope, path) else {
                return;
            };
            let next = !truthy(get_path(data, &target));
            set_path(data, &target, Json::Bool(next));
        }
        ClientOp::Append { path, value } => {
            let Some(target) = abs_data_path(scope, path) else {
                return;
            };
            let resolved = resolve_set_value(data, scope, value);
            let mut arr = match get_path(data, &target) {
                Json::Array(a) => a.clone(),
                _ => Vec::new(),
            };
            arr.push(resolved);
            set_path(data, &target, Json::Array(arr));
        }
        ClientOp::RemoveAt { path, index } => {
            let Some(target) = abs_data_path(scope, path) else {
                return;
            };
            if let Json::Array(a) = get_path(data, &target) {
                if *index < a.len() {
                    let mut arr = a.clone();
                    arr.remove(*index);
                    set_path(data, &target, Json::Array(arr));
                }
            }
        }
        // View-control ops (navigate/tabs/dialogs/timers) are not data ops.
        ClientOp::Navigate { .. }
        | ClientOp::SelectTab { .. }
        | ClientOp::OpenDialog { .. }
        | ClientOp::CloseDialog { .. }
        | ClientOp::StartTimer { .. }
        | ClientOp::PauseTimer { .. }
        | ClientOp::ResetTimer { .. } => {}
    }
}

/// Resolve a [`ClientOp::Set`]/`Append` value: a literal, `{"$path":"a.b"}`
/// (copy from another scope-resolved state path), or a string carrying
/// `{{path}}` references. Mirrors the server's tool-arg interpolation: a string
/// that is *exactly* one reference (`"{{item.id}}"`) yields the raw typed value
/// (a number stays a number), a mixed string splices display text, and arrays/
/// objects resolve recursively.
fn resolve_set_value(data: &Json, scope: &Scope, value: &Json) -> Json {
    match value {
        Json::Object(map) => {
            if map.len() == 1 {
                if let Some(Json::String(p)) = map.get("$path") {
                    return resolve_value(scope, data, p);
                }
            }
            Json::Object(
                map.iter()
                    .map(|(k, v)| (k.clone(), resolve_set_value(data, scope, v)))
                    .collect(),
            )
        }
        Json::Array(items) => Json::Array(
            items
                .iter()
                .map(|v| resolve_set_value(data, scope, v))
                .collect(),
        ),
        Json::String(s) if s.contains("{{") => match whole_reference(s.trim()) {
            Some(path) => resolve_value(scope, data, path),
            None => Json::String(interpolate(s, data, scope)),
        },
        literal => literal.clone(),
    }
}

/// If `s` is exactly one `{{ path }}` reference, return its trimmed path.
fn whole_reference(s: &str) -> Option<&str> {
    let inner = s.strip_prefix("{{")?.strip_suffix("}}")?;
    if inner.contains("{{") || inner.contains("}}") {
        return None;
    }
    let inner = inner.trim();
    (!inner.is_empty()).then_some(inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scope() -> Scope {
        Scope::default()
    }

    #[test]
    fn set_toggle_append_remove() {
        let mut d = json!({});
        apply_data_op(
            &mut d,
            &scope(),
            &ClientOp::Set {
                path: "form.name".into(),
                value: json!("Jane"),
            },
        );
        assert_eq!(get_path(&d, "form.name"), &json!("Jane"));

        apply_data_op(
            &mut d,
            &scope(),
            &ClientOp::Toggle {
                path: "open".into(),
            },
        );
        assert_eq!(get_path(&d, "open"), &json!(true));
        apply_data_op(
            &mut d,
            &scope(),
            &ClientOp::Toggle {
                path: "open".into(),
            },
        );
        assert_eq!(get_path(&d, "open"), &json!(false));

        apply_data_op(
            &mut d,
            &scope(),
            &ClientOp::Append {
                path: "items".into(),
                value: json!("a"),
            },
        );
        apply_data_op(
            &mut d,
            &scope(),
            &ClientOp::Append {
                path: "items".into(),
                value: json!("b"),
            },
        );
        assert_eq!(get_path(&d, "items"), &json!(["a", "b"]));
        apply_data_op(
            &mut d,
            &scope(),
            &ClientOp::RemoveAt {
                path: "items".into(),
                index: 0,
            },
        );
        assert_eq!(get_path(&d, "items"), &json!(["b"]));
    }

    #[test]
    fn set_value_copy_directive() {
        let mut d = json!({ "draft": "hello" });
        apply_data_op(
            &mut d,
            &scope(),
            &ClientOp::Set {
                path: "saved".into(),
                value: json!({ "$path": "draft" }),
            },
        );
        assert_eq!(get_path(&d, "saved"), &json!("hello"));
    }

    #[test]
    fn set_value_interpolates_templates() {
        // The recipe-browser pattern: a for_each row button stashes the row's
        // id, then navigates — `{{item.x}}` must resolve against the row scope.
        let mut d = json!({ "recipes": [{ "id": "pad-thai", "servings": 4 }] });
        let row = scope().with_item("recipe", "recipes.0".to_string());
        apply_data_op(
            &mut d,
            &row,
            &ClientOp::Set {
                path: "selectedId".into(),
                value: json!("{{recipe.id}}"),
            },
        );
        assert_eq!(get_path(&d, "selectedId"), &json!("pad-thai"));

        // A whole reference keeps the raw type; a mixed string splices text.
        apply_data_op(
            &mut d,
            &row,
            &ClientOp::Set {
                path: "n".into(),
                value: json!("{{ recipe.servings }}"),
            },
        );
        assert_eq!(get_path(&d, "n"), &json!(4));
        apply_data_op(
            &mut d,
            &row,
            &ClientOp::Set {
                path: "label".into(),
                value: json!("serves {{recipe.servings}}"),
            },
        );
        assert_eq!(get_path(&d, "label"), &json!("serves 4"));

        // References resolve recursively inside containers (append included).
        apply_data_op(
            &mut d,
            &row,
            &ClientOp::Append {
                path: "picked".into(),
                value: json!({ "id": "{{recipe.id}}", "note": "" }),
            },
        );
        assert_eq!(
            get_path(&d, "picked"),
            &json!([{ "id": "pad-thai", "note": "" }])
        );

        // No references → stored verbatim (a literal stays a literal).
        apply_data_op(
            &mut d,
            &scope(),
            &ClientOp::Set {
                path: "plain".into(),
                value: json!("no braces"),
            },
        );
        assert_eq!(get_path(&d, "plain"), &json!("no braces"));
    }

    #[test]
    fn timer_state_math() {
        let mut t = TimerState::default();
        assert!(!t.running());
        assert_eq!(t.elapsed_ms(100.0), 0.0);

        // start → running, elapsed grows with `now`.
        t.start(1_000.0);
        assert!(t.running());
        assert_eq!(t.elapsed_ms(1_250.0), 250.0);
        // A second start while running is a no-op (keeps the original origin).
        t.start(9_999.0);
        assert_eq!(t.elapsed_ms(1_250.0), 250.0);

        // pause folds the run into base; elapsed freezes.
        t.pause(1_500.0);
        assert!(!t.running());
        assert_eq!(t.elapsed_ms(9_999.0), 500.0);

        // resume accumulates on top of base.
        t.start(2_000.0);
        assert_eq!(t.elapsed_ms(2_100.0), 600.0);

        // finish stops a running timer exactly at the target and fires once.
        assert!(t.finish(30_000.0));
        assert!(!t.running());
        assert_eq!(t.elapsed_ms(99_999.0), 30_000.0);
        assert!(!t.finish(30_000.0), "finish must be once-only");

        // reset zeroes everything.
        t.reset();
        assert_eq!(t, TimerState::default());

        // A clock that jumps backwards never yields negative elapsed time.
        t.start(5_000.0);
        assert_eq!(t.elapsed_ms(4_000.0), 0.0);
        t.pause(4_000.0);
        assert_eq!(t.elapsed_ms(9_000.0), 0.0);
    }

    #[test]
    fn validation_rules() {
        assert!(!rule_passes(&ValidationKind::Required, &json!("")));
        assert!(rule_passes(&ValidationKind::Required, &json!("x")));
        assert!(!rule_passes(&ValidationKind::MinLen { n: 3 }, &json!("ab")));
        assert!(rule_passes(&ValidationKind::MinLen { n: 3 }, &json!("abc")));
        assert!(!rule_passes(
            &ValidationKind::MaxLen { n: 2 },
            &json!("abc")
        ));
        assert!(rule_passes(
            &ValidationKind::Range {
                min: Some(1.0),
                max: Some(5.0)
            },
            &json!(3)
        ));
        assert!(!rule_passes(
            &ValidationKind::Range {
                min: Some(1.0),
                max: Some(5.0)
            },
            &json!(9)
        ));
        // Deferred kinds always pass client-side.
        assert!(rule_passes(
            &ValidationKind::Pattern {
                regex: "^x$".into()
            },
            &json!("y")
        ));
    }
}
