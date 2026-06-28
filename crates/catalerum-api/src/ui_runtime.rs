//! Server-side execution of emerged-UI event handlers (SOUL §12, plan P3/P4).
//!
//! The web interpreter renders a [`UiSpec`] and applies the cheap client ops
//! locally; anything authority-bearing — a [`Handler::Tool`], a
//! [`Handler::Script`] (Boa) — round-trips to `POST /uis/{id}/event`, which
//! lands here. [`run_handler`] resolves the fired node's handler and returns a
//! flat `Vec` of [`UiAction`](catalerum_core::model_ui::UiAction)-shaped JSON the
//! client applies verbatim. Every [`ClientOp`] (including `toggle`/`append`/
//! `remove_at`) is **folded server-side** against the posted state snapshot into
//! concrete `set` actions, so the client only ever applies the closed `UiAction`
//! vocabulary.
//!
//! ## Trust boundary
//! An emerged UI is strictly **less** powerful than chat (SOUL §13/§19). Two gates
//! apply to every tool a handler reaches — the declarative [`Handler::Tool`] *and*
//! a script's `catalerum.callTool`:
//! 1. **Allow-list** — the tool must be on `[ui].handler_tools`
//!    ([`UiConfig`](crate::config::UiConfig)), re-checked here at dispatch, not
//!    only at authoring. This is the ceiling even for a wildcard Owner.
//! 2. **Capability cap** — the [`ToolContext`] carries the firing user's *own*
//!    role capabilities, so dispatch denies a write a Viewer could not perform.
//!
//! A script additionally may not call a **confirm-required** tool — one whose
//! `required_capability()` exceeds base-Member authority (delete / exec / egress /
//! channel). Those are reachable only through the declarative `tool` handler, never
//! mid-script (a confirm round-trip would force a non-idempotent re-run). Since the
//! default allow-list already excludes them, this is belt-and-suspenders that also
//! holds if an admin widens the list.

use std::collections::HashSet;
use std::sync::Arc;

use serde_json::{json, Value};

use catalerum_core::capability::Capability;
use catalerum_core::model::Map;
use catalerum_core::model_ui::{
    get_path, set_path, stringify, truthy, ClientOp, EventName, Handler, UiNode, UiSpec,
};
use catalerum_core::tool::{ToolContext, ToolRegistry};
use catalerum_iam::Role;
use catalerum_script::{ScriptCodeRunner, UiScriptHost};

use crate::error::{ApiError, ApiResult};

/// Who is firing a handler and what they may reach: the tool registry, the
/// `[ui].handler_tools` allow-list, and the firing user's capped [`ToolContext`].
/// Built once per `POST /uis/{id}/event` and shared by both handler kinds.
pub struct Dispatcher {
    /// The shared tool registry (SOUL §7).
    pub registry: ToolRegistry,
    /// Tools a UI handler may call (the runtime ceiling, even for an Owner).
    pub allow: Arc<HashSet<String>>,
    /// The firing user's context — capped to their own role capabilities.
    pub ctx: ToolContext,
}

/// Run the handler bound to `event` on node `node_id` of `spec`, returning the
/// client actions to apply (each a `UiAction`-shaped JSON object) **and** the
/// resulting transient state (so the caller can recompute `computed.*` against
/// it). `client_state` is the posted transient-state snapshot; `scope` is the
/// firing node's `for_each` bindings (`{}` when none), overlaid onto the state for
/// `{{path}}` interpolation and handed to scripts as `input.scope`. `ctx` must
/// already carry the firing user's capability set (the cap gate at dispatch).
pub async fn run_handler(
    dispatcher: &Dispatcher,
    spec: &UiSpec,
    node_id: &str,
    event: EventName,
    client_state: Value,
    scope: Value,
) -> ApiResult<(Vec<Value>, Value)> {
    let Dispatcher {
        registry,
        allow,
        ctx,
    } = dispatcher;

    let node = find_node_in_spec(spec, node_id)
        .ok_or_else(|| ApiError::bad_request(format!("no node `{node_id}` in this UI")))?;
    let handler = node.events.get(&event).ok_or_else(|| {
        ApiError::bad_request(format!("node `{node_id}` has no handler for this event"))
    })?;

    // The working state ops fold against; the interpolation context overlays the
    // for_each scope on top of it (scope vars shadow state keys).
    let mut working = client_state.clone();
    let interp_ctx = overlay(&client_state, &scope);

    match handler {
        // A client handler reaching the server (uniform-post client): fold its ops.
        Handler::Client { ops } => {
            let mut actions = Vec::new();
            for op in ops {
                fold_client_op(&mut working, &scope, op, &mut actions);
            }
            Ok((actions, working))
        }

        Handler::Tool {
            tool,
            args,
            result_path,
            then,
        } => {
            gate_dispatchable(registry, tool, allow.as_ref(), false)?;
            let call_args = interpolate(&map_to_value(args), &interp_ctx);
            let result = registry.dispatch(tool, call_args, ctx).await?;

            let mut actions = Vec::new();
            if let Some(path) = result_path {
                set_path(&mut working, path, result.clone());
                actions.push(json!({ "op": "set", "path": path, "value": result }));
            }
            for op in then {
                fold_client_op(&mut working, &scope, op, &mut actions);
            }
            Ok((actions, working))
        }

        Handler::Script { handler } => {
            let script = spec.scripts.get(handler).ok_or_else(|| {
                ApiError::bad_request(format!("UI references unknown script `{handler}`"))
            })?;
            let input = json!({
                "event": { "node": node_id, "name": event },
                "scope": scope,
            });
            let outcome = ScriptCodeRunner::new()
                .run_ui_script(
                    &script.source,
                    &input,
                    &client_state,
                    event_host(dispatcher),
                )
                .await
                .map_err(|e| ApiError::bad_request(format!("script `{handler}`: {e}")))?;
            Ok((outcome.actions, outcome.state))
        }

        // AI handlers are relayed into chat client-side (SOUL §12); nothing to run.
        Handler::Ai { .. } => Ok((Vec::new(), client_state)),
    }
}

/// Evaluate every [`ComputedDef`](catalerum_core::model_ui::ComputedDef) of `spec`
/// against `state`, returning the `{ <name>: <value> }` object exposed to bindings
/// at `computed.<name>` (SOUL §12). Each computed script receives `{ state }` (and
/// `catalerum.getState()`); a stale `computed` key is stripped first so a derived
/// value never feeds on a previous round's output. Empty `computed` → an empty
/// object.
pub async fn run_computed(
    dispatcher: &Dispatcher,
    spec: &UiSpec,
    mut state: Value,
) -> ApiResult<Value> {
    if let Some(obj) = state.as_object_mut() {
        obj.remove("computed");
    }
    let mut out = serde_json::Map::new();
    for def in &spec.computed {
        let script = spec.scripts.get(&def.handler).ok_or_else(|| {
            ApiError::bad_request(format!(
                "computed `{}` references unknown script `{}`",
                def.name, def.handler
            ))
        })?;
        let input = json!({ "state": &state });
        let outcome = ScriptCodeRunner::new()
            .run_ui_script(&script.source, &input, &state, event_host(dispatcher))
            .await
            .map_err(|e| ApiError::bad_request(format!("computed `{}`: {e}", def.name)))?;
        out.insert(def.name.clone(), outcome.returned);
    }
    Ok(Value::Object(out))
}

/// Run a named [`ValidationKind::Script`](catalerum_core::model_ui::ValidationKind::Script)
/// rule (SOUL §12): evaluate `handler` with `{ value, state }` bound and return
/// its `{ ok, message? }` result verbatim. Like a script handler it may reach the
/// host bridge (e.g. a uniqueness check via an allow-listed read tool).
pub async fn run_validation(
    dispatcher: &Dispatcher,
    spec: &UiSpec,
    handler: &str,
    value: Value,
    state: Value,
) -> ApiResult<Value> {
    let script = spec.scripts.get(handler).ok_or_else(|| {
        ApiError::bad_request(format!(
            "UI references unknown validation script `{handler}`"
        ))
    })?;
    let input = json!({ "value": value, "state": &state });
    let outcome = ScriptCodeRunner::new()
        .run_ui_script(&script.source, &input, &state, event_host(dispatcher))
        .await
        .map_err(|e| ApiError::bad_request(format!("validation script `{handler}`: {e}")))?;
    Ok(outcome.returned)
}

/// Build the [`EventHost`] bridge for `dispatcher` (the firing user's grant).
fn event_host(dispatcher: &Dispatcher) -> Arc<EventHost> {
    Arc::new(EventHost {
        registry: dispatcher.registry.clone(),
        handle: tokio::runtime::Handle::current(),
        ctx: dispatcher.ctx.clone(),
        allow: dispatcher.allow.clone(),
        confirm_floor: catalerum_iam::base_capabilities(Role::Member),
    })
}

// ---------------------------------------------------------------------------
// Host bridge — the only authority a UI script touches (`catalerum.callTool`)
// ---------------------------------------------------------------------------

/// The [`UiScriptHost`] for one event: `call_tool` re-applies the allow-list and
/// confirm-tool exclusion, then `block_on`s the async [`ToolRegistry::dispatch`]
/// (valid — it runs on the script's `spawn_blocking` thread, never a runtime
/// worker) under the firing user's capped `ctx`.
struct EventHost {
    registry: ToolRegistry,
    handle: tokio::runtime::Handle,
    ctx: ToolContext,
    allow: Arc<HashSet<String>>,
    /// Base-Member authority: a tool requiring more than this is confirm-required
    /// and not script-callable.
    confirm_floor: Vec<Capability>,
}

impl UiScriptHost for EventHost {
    fn call_tool(&self, tool: &str, args: Value) -> Result<Value, String> {
        gate_dispatchable(&self.registry, tool, self.allow.as_ref(), true)
            .map_err(|e| e.to_string())?;
        if is_confirm_required(&self.registry, tool, &self.confirm_floor) {
            return Err(format!(
                "tool `{tool}` requires confirmation and cannot be called from a script; \
                 use a `tool` handler"
            ));
        }
        self.handle
            .block_on(self.registry.dispatch(tool, args, &self.ctx))
            .map_err(|e| e.to_string())
    }
}

/// Reject a tool a handler may not dispatch: unknown, or not on the UI allow-list.
/// `from_script` only tunes the message.
pub(crate) fn gate_dispatchable(
    registry: &ToolRegistry,
    tool: &str,
    allow: &HashSet<String>,
    from_script: bool,
) -> ApiResult<()> {
    if !allow.contains(tool) {
        let how = if from_script {
            "a UI script"
        } else {
            "a UI handler"
        };
        return Err(ApiError::Forbidden(format!(
            "tool `{tool}` is not on the UI handler allow-list and cannot be called from {how}"
        )));
    }
    if !registry.contains(tool) {
        return Err(ApiError::bad_request(format!("unknown tool `{tool}`")));
    }
    Ok(())
}

/// Whether `tool`'s `required_capability()` exceeds base-Member authority — the
/// derived "confirm-required" predicate (delete / exec / egress / channel).
fn is_confirm_required(registry: &ToolRegistry, tool: &str, floor: &[Capability]) -> bool {
    let Some(t) = registry.get(tool) else {
        return false;
    };
    match t.required_capability() {
        Some(req) => !floor.iter().any(|held| held.covers(&req)),
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Client-op fold + interpolation (pure)
// ---------------------------------------------------------------------------

/// Apply one [`ClientOp`] to the working `state` and emit the equivalent
/// `UiAction` JSON. Data ops (`toggle`/`append`/`remove_at`) are resolved against
/// the current state into a concrete `set`, so the client applies only `set` /
/// `navigate` / `open_dialog` / `close_dialog`. `scope` is the firing node's
/// resolved `for_each` bindings, shadowing state keys in `$path`/`{{path}}`
/// value resolution (the same rules as the client reducer).
fn fold_client_op(state: &mut Value, scope: &Value, op: &ClientOp, out: &mut Vec<Value>) {
    match op {
        ClientOp::Set { path, value } => {
            let resolved = resolve_copy(state, scope, value);
            set_path(state, path, resolved.clone());
            out.push(json!({ "op": "set", "path": path, "value": resolved }));
        }
        ClientOp::Toggle { path } => {
            let next = !truthy(get_path(state, path));
            set_path(state, path, json!(next));
            out.push(json!({ "op": "set", "path": path, "value": next }));
        }
        ClientOp::Navigate { view } => out.push(json!({ "op": "navigate", "view": view })),
        ClientOp::SelectTab { id, index } => {
            out.push(json!({ "op": "select_tab", "id": id, "index": index }));
        }
        ClientOp::OpenDialog { id } => out.push(json!({ "op": "open_dialog", "id": id })),
        ClientOp::CloseDialog { id } => out.push(json!({ "op": "close_dialog", "id": id })),
        // Timer controls are view-state ops like `select_tab`: no `state` fold,
        // just a verbatim action for the client's timer reducer.
        ClientOp::StartTimer { id } => out.push(json!({ "op": "start_timer", "id": id })),
        ClientOp::PauseTimer { id } => out.push(json!({ "op": "pause_timer", "id": id })),
        ClientOp::ResetTimer { id } => out.push(json!({ "op": "reset_timer", "id": id })),
        ClientOp::Append { path, value } => {
            let resolved = resolve_copy(state, scope, value);
            let mut arr = array_at(state, path);
            arr.push(resolved);
            let arr = Value::Array(arr);
            set_path(state, path, arr.clone());
            out.push(json!({ "op": "set", "path": path, "value": arr }));
        }
        ClientOp::RemoveAt { path, index } => {
            let mut arr = array_at(state, path);
            if *index < arr.len() {
                arr.remove(*index);
            }
            let arr = Value::Array(arr);
            set_path(state, path, arr.clone());
            out.push(json!({ "op": "set", "path": path, "value": arr }));
        }
    }
}

/// Read the array at `path` (a fresh clone; non-arrays/missing → empty).
fn array_at(state: &Value, path: &str) -> Vec<Value> {
    match get_path(state, path) {
        Value::Array(a) => a.clone(),
        _ => Vec::new(),
    }
}

/// Resolve a [`ClientOp::Set`] value: `{"$path":"a.b"}` copies from another state
/// path; anything else is a literal.
/// Resolve a `set`/`append` value against `state` with `scope` shadowing: a
/// `{"$path":…}` object copies the raw value at that path, a string carrying
/// `{{path}}` references interpolates (a whole reference keeps the raw type),
/// and containers resolve recursively — mirroring the client reducer and tool
/// args, so a spec behaves the same whichever side folds the op.
fn resolve_copy(state: &Value, scope: &Value, value: &Value) -> Value {
    resolve_copy_in(&overlay(state, scope), value)
}

fn resolve_copy_in(ctx: &Value, value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            if map.len() == 1 {
                if let Some(Value::String(path)) = map.get("$path") {
                    return get_path(ctx, path).clone();
                }
            }
            Value::Object(
                map.iter()
                    .map(|(k, v)| (k.clone(), resolve_copy_in(ctx, v)))
                    .collect(),
            )
        }
        Value::Array(items) => {
            Value::Array(items.iter().map(|v| resolve_copy_in(ctx, v)).collect())
        }
        Value::String(s) if s.contains("{{") => interpolate_str(s, ctx),
        literal => literal.clone(),
    }
}

/// `base` with `overlay`'s top-level keys layered on (used so a `for_each` item /
/// index shadows state during `{{path}}` interpolation).
fn overlay(base: &Value, overlay: &Value) -> Value {
    let mut obj = base.as_object().cloned().unwrap_or_default();
    if let Some(o) = overlay.as_object() {
        for (k, v) in o {
            obj.insert(k.clone(), v.clone());
        }
    }
    Value::Object(obj)
}

/// A [`model::Map`](catalerum_core::model::Map) (a `BTreeMap`) as a JSON object.
fn map_to_value(map: &Map) -> Value {
    Value::Object(map.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
}

/// Interpolate `{{path}}` references in every string within `value` against
/// `ctx`. A string that is *exactly* one reference (`"{{n}}"`) yields the raw
/// typed value at that path (so a number stays a number); a mixed string splices
/// each reference as display text.
fn interpolate(value: &Value, ctx: &Value) -> Value {
    match value {
        Value::String(s) => interpolate_str(s, ctx),
        Value::Array(a) => Value::Array(a.iter().map(|v| interpolate(v, ctx)).collect()),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, v)| (k.clone(), interpolate(v, ctx)))
                .collect(),
        ),
        scalar => scalar.clone(),
    }
}

fn interpolate_str(s: &str, ctx: &Value) -> Value {
    if let Some(path) = whole_reference(s.trim()) {
        return get_path(ctx, path).clone();
    }
    Value::String(splice(s, ctx))
}

/// If `s` is exactly one `{{ path }}` reference, return its trimmed path.
fn whole_reference(s: &str) -> Option<&str> {
    let inner = s.strip_prefix("{{")?.strip_suffix("}}")?;
    if inner.contains("{{") || inner.contains("}}") {
        return None;
    }
    Some(inner.trim())
}

/// Replace every `{{ path }}` in `s` with the display text of `ctx` at `path`.
fn splice(s: &str, ctx: &Value) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("}}") {
            Some(end) => {
                out.push_str(&stringify(get_path(ctx, after[..end].trim())));
                rest = &after[end + 2..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// Find a node by id anywhere in the spec's view trees.
pub(crate) fn find_node_in_spec<'a>(spec: &'a UiSpec, id: &str) -> Option<&'a UiNode> {
    spec.views.iter().find_map(|v| find_node(&v.root, id))
}

fn find_node<'a>(node: &'a UiNode, id: &str) -> Option<&'a UiNode> {
    if node.id == id {
        return Some(node);
    }
    node.children.iter().find_map(|c| find_node(c, id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Value {
        json!({ "form": { "n": 3 }, "item": { "id": "x1", "label": "First" } })
    }

    #[test]
    fn whole_reference_preserves_typed_value() {
        // A lone reference keeps the number a number, not "3".
        assert_eq!(interpolate(&json!("{{form.n}}"), &ctx()), json!(3));
        assert_eq!(interpolate(&json!("{{ item.id }}"), &ctx()), json!("x1"));
        // Missing path → null.
        assert_eq!(interpolate(&json!("{{nope}}"), &ctx()), Value::Null);
    }

    #[test]
    fn mixed_string_splices_as_text() {
        assert_eq!(
            interpolate(&json!("Hi {{item.label}} (#{{form.n}})"), &ctx()),
            json!("Hi First (#3)")
        );
    }

    #[test]
    fn interpolate_recurses_into_objects_and_arrays() {
        let args = json!({ "title": "{{item.label}}", "tags": ["{{item.id}}", "lit"] });
        assert_eq!(
            interpolate(&args, &ctx()),
            json!({ "title": "First", "tags": ["x1", "lit"] })
        );
    }

    #[test]
    fn fold_toggle_and_append_resolve_to_set() {
        let mut state = json!({ "open": false, "items": [1] });
        let mut out = Vec::new();
        fold_client_op(
            &mut state,
            &json!({}),
            &ClientOp::Toggle {
                path: "open".into(),
            },
            &mut out,
        );
        fold_client_op(
            &mut state,
            &json!({}),
            &ClientOp::Append {
                path: "items".into(),
                value: json!(2),
            },
            &mut out,
        );
        assert_eq!(
            out,
            vec![
                json!({ "op": "set", "path": "open", "value": true }),
                json!({ "op": "set", "path": "items", "value": [1, 2] }),
            ]
        );
        assert_eq!(state, json!({ "open": true, "items": [1, 2] }));
    }

    #[test]
    fn fold_remove_at_and_copy_set() {
        let mut state = json!({ "list": ["a", "b", "c"], "src": "copied" });
        let mut out = Vec::new();
        fold_client_op(
            &mut state,
            &json!({}),
            &ClientOp::RemoveAt {
                path: "list".into(),
                index: 1,
            },
            &mut out,
        );
        fold_client_op(
            &mut state,
            &json!({}),
            &ClientOp::Set {
                path: "dst".into(),
                value: json!({ "$path": "src" }),
            },
            &mut out,
        );
        assert_eq!(state["list"], json!(["a", "c"]));
        assert_eq!(state["dst"], json!("copied"));
        assert_eq!(
            out[1],
            json!({ "op": "set", "path": "dst", "value": "copied" })
        );
    }

    #[test]
    fn fold_set_interpolates_templates_with_scope_shadowing() {
        // Mirrors the client reducer: a whole `{{ref}}` keeps the raw type and
        // scope vars shadow state; a mixed string splices; containers recurse.
        let mut state = json!({ "recipe": "stale", "n": 7 });
        let scope = json!({ "recipe": { "id": "pad-thai", "servings": 4 } });
        let mut out = Vec::new();
        fold_client_op(
            &mut state,
            &scope,
            &ClientOp::Set {
                path: "selectedId".into(),
                value: json!("{{recipe.id}}"),
            },
            &mut out,
        );
        fold_client_op(
            &mut state,
            &scope,
            &ClientOp::Set {
                path: "picked".into(),
                value: json!({ "id": "{{recipe.id}}", "label": "serves {{recipe.servings}}" }),
            },
            &mut out,
        );
        assert_eq!(state["selectedId"], json!("pad-thai"));
        assert_eq!(
            state["picked"],
            json!({ "id": "pad-thai", "label": "serves 4" })
        );
        // A braceless literal is untouched even when it names a state key.
        fold_client_op(
            &mut state,
            &scope,
            &ClientOp::Set {
                path: "plain".into(),
                value: json!("n"),
            },
            &mut out,
        );
        assert_eq!(state["plain"], json!("n"));
    }

    #[test]
    fn fold_timer_ops_emit_passthrough_actions() {
        // Timer ops never touch `state` — they forward verbatim to the client's
        // timer reducer, exactly like `select_tab`.
        let mut state = json!({ "n": 1 });
        let mut out = Vec::new();
        for op in [
            ClientOp::StartTimer { id: "t".into() },
            ClientOp::PauseTimer { id: "t".into() },
            ClientOp::ResetTimer { id: "t".into() },
        ] {
            fold_client_op(&mut state, &json!({}), &op, &mut out);
        }
        assert_eq!(
            out,
            vec![
                json!({ "op": "start_timer", "id": "t" }),
                json!({ "op": "pause_timer", "id": "t" }),
                json!({ "op": "reset_timer", "id": "t" }),
            ]
        );
        assert_eq!(state, json!({ "n": 1 }));
    }

    #[test]
    fn fold_select_tab_emits_passthrough_action() {
        // `select_tab` is a view-control op: it does not touch `state`, it just
        // forwards a verbatim action for the client to apply.
        let mut state = json!({ "n": 1 });
        let mut out = Vec::new();
        fold_client_op(
            &mut state,
            &json!({}),
            &ClientOp::SelectTab {
                id: "tabs".into(),
                index: 2,
            },
            &mut out,
        );
        assert_eq!(
            out,
            vec![json!({ "op": "select_tab", "id": "tabs", "index": 2 })]
        );
        assert_eq!(state, json!({ "n": 1 }));
    }

    #[test]
    fn overlay_scope_shadows_state() {
        let merged = overlay(
            &json!({ "item": "stale", "keep": 1 }),
            &json!({ "item": "fresh" }),
        );
        assert_eq!(merged, json!({ "item": "fresh", "keep": 1 }));
    }

    // --- end-to-end handler dispatch --------------------------------------

    use catalerum_core::tool::Tool;

    /// A capability-free tool that echoes its arguments — enough to assert the
    /// dispatch + result-path wiring without a real tool.
    struct EchoTool;

    #[async_trait::async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn parameters_schema(&self) -> Value {
            json!({ "type": "object" })
        }
        async fn invoke(&self, args: Value, _ctx: &ToolContext) -> catalerum_core::Result<Value> {
            Ok(args)
        }
    }

    fn dispatcher_with_echo() -> Dispatcher {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(EchoTool));
        Dispatcher {
            registry,
            allow: Arc::new(["echo".to_string()].into_iter().collect()),
            ctx: ToolContext {
                capabilities: Some(catalerum_iam::base_capabilities(Role::Member)),
                ..ToolContext::default()
            },
        }
    }

    /// A `tool` handler interpolates its args against the posted state, dispatches
    /// the allow-listed tool, and writes the result to `result_path` as a `set`.
    #[tokio::test]
    async fn tool_handler_dispatches_and_writes_result() {
        let spec: UiSpec = serde_json::from_value(json!({
            "default_view": "main",
            "views": [{ "id": "main", "title": "M", "root": {
                "id": "btn", "kind": "button",
                "events": { "click": {
                    "kind": "tool", "tool": "echo",
                    "args": { "x": "{{form.v}}" }, "result_path": "out"
                } }
            } }]
        }))
        .unwrap();

        let (actions, _) = run_handler(
            &dispatcher_with_echo(),
            &spec,
            "btn",
            EventName::Click,
            json!({ "form": { "v": 42 } }),
            json!({}),
        )
        .await
        .expect("handler runs");
        // The `{{form.v}}` reference stays a number, echoed back under `out`.
        assert_eq!(
            actions,
            vec![json!({ "op": "set", "path": "out", "value": { "x": 42 } })]
        );
    }

    /// A `load` lifecycle event fired on a view's root container dispatches like
    /// any other event — the client fires it on mount/navigate so an App can pull
    /// its durable data (e.g. `app_data_list`) into state before first paint.
    #[tokio::test]
    async fn load_event_on_view_root_dispatches() {
        let spec: UiSpec = serde_json::from_value(json!({
            "default_view": "main",
            "views": [{ "id": "main", "title": "M", "root": {
                "id": "root", "kind": "stack",
                "events": { "load": {
                    "kind": "tool", "tool": "echo",
                    "args": { "limit": 100 }, "result_path": "stored"
                } }
            } }]
        }))
        .unwrap();

        let (actions, _) = run_handler(
            &dispatcher_with_echo(),
            &spec,
            "root",
            EventName::Load,
            json!({}),
            json!({}),
        )
        .await
        .expect("load handler runs");
        assert_eq!(
            actions,
            vec![json!({ "op": "set", "path": "stored", "value": { "limit": 100 } })]
        );
    }

    /// A not-allow-listed tool is rejected at dispatch even though the spec named
    /// it (the runtime ceiling).
    #[tokio::test]
    async fn tool_handler_rejects_non_allowlisted() {
        let spec: UiSpec = serde_json::from_value(json!({
            "default_view": "main",
            "views": [{ "id": "main", "title": "M", "root": {
                "id": "btn", "kind": "button",
                "events": { "click": { "kind": "tool", "tool": "echo" } }
            } }]
        }))
        .unwrap();
        let mut d = dispatcher_with_echo();
        d.allow = Arc::new(HashSet::new()); // empty allow-list
        let err = run_handler(&d, &spec, "btn", EventName::Click, json!({}), json!({}))
            .await
            .expect_err("not allow-listed");
        assert!(matches!(err, ApiError::Forbidden(_)), "got {err:?}");
    }

    /// A `script` handler reaches `catalerum.callTool` through the host bridge and
    /// its `setState` surfaces as a `set` action. Needs a multi-thread runtime:
    /// the script's `block_on` runs on a `spawn_blocking` thread.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn script_handler_calls_tool_through_bridge() {
        let spec: UiSpec = serde_json::from_value(json!({
            "default_view": "main",
            "views": [{ "id": "main", "title": "M", "root": {
                "id": "btn", "kind": "button",
                "events": { "click": { "kind": "script", "handler": "go" } }
            } }],
            "scripts": { "go": {
                "runtime": "javascript",
                "source": "var r = catalerum.callTool('echo', { hi: 7 }); catalerum.setState({ got: r.hi });"
            } }
        }))
        .unwrap();

        let (actions, _) = run_handler(
            &dispatcher_with_echo(),
            &spec,
            "btn",
            EventName::Click,
            json!({}),
            json!({}),
        )
        .await
        .expect("script runs");
        assert!(
            actions.contains(&json!({ "op": "set", "path": "got", "value": 7 })),
            "actions: {actions:?}"
        );
    }

    /// `run_computed` evaluates each `ComputedDef` against the state and returns a
    /// `{ name: value }` object; a stale `computed` key is ignored.
    #[tokio::test]
    async fn computed_evaluates_each_def() {
        let spec: UiSpec = serde_json::from_value(json!({
            "default_view": "main",
            "views": [{ "id": "main", "title": "M", "root": { "id": "t", "kind": "text" } }],
            "computed": [
                { "name": "count", "handler": "count_items" },
                { "name": "greeting", "handler": "greet" }
            ],
            "scripts": {
                "count_items": { "runtime": "javascript", "source": "return (input.state.items || []).length;" },
                "greet": { "runtime": "javascript", "source": "return 'hi ' + input.state.name;" }
            }
        }))
        .unwrap();

        let computed = run_computed(
            &dispatcher_with_echo(),
            &spec,
            json!({ "items": [1, 2, 3], "name": "ada", "computed": { "stale": true } }),
        )
        .await
        .expect("computes");
        assert_eq!(computed, json!({ "count": 3, "greeting": "hi ada" }));
    }

    /// A `ValidationKind::Script` rule runs the named script over `{value, state}`
    /// and returns its `{ ok, message? }` verbatim.
    #[tokio::test]
    async fn validation_script_returns_ok_and_message() {
        let spec: UiSpec = serde_json::from_value(json!({
            "default_view": "main",
            "views": [{ "id": "main", "title": "M", "root": { "id": "f", "kind": "text_input" } }],
            "scripts": { "min3": {
                "runtime": "javascript",
                "source": "return { ok: String(input.value).length >= 3, message: 'too short' };"
            } }
        }))
        .unwrap();
        let d = dispatcher_with_echo();

        let good = run_validation(&d, &spec, "min3", json!("abcd"), json!({}))
            .await
            .expect("validates");
        assert_eq!(good, json!({ "ok": true, "message": "too short" }));

        let bad = run_validation(&d, &spec, "min3", json!("ab"), json!({}))
            .await
            .expect("validates");
        assert_eq!(bad.get("ok"), Some(&json!(false)));
    }
}
