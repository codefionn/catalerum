//! Per-App durable key/value tools (SOUL §12/§29).

use super::*;

/// Resolve the `app_data` namespace a call operates in (SOUL §12/§29).
///
/// - **From an App event handler** ([`ToolContext::ui_id`] is set): the namespace
///   is *forced* to the firing App — or, for a **sub-app** of a shell suite, to
///   the topmost verified ancestor (see [`shared_app_namespace`]); any
///   caller-supplied `app` argument is ignored. This is the isolation boundary,
///   so one App can never reach an unrelated App's keys.
/// - **From chat / an automation / MCP** (no `ui_id`): the caller names the target
///   App via a required `app` argument — the "automation collects → App presents"
///   path (SOUL §12), where a trusted full-authority caller writes into an App's
///   namespace so the App can present it.
pub(crate) async fn app_data_namespace(
    store: &Store,
    ctx: &ToolContext,
    args: &Json,
) -> Result<String> {
    if let Some(ui_id) = ctx.ui_id {
        let ws = workspace(ctx)?;
        return Ok(shared_app_namespace(store, ws, ui_id).await);
    }
    required_str(args, "app").map_err(|_| {
        Error::invalid(
            "`app` is required when not called from an App handler — name the App's \
             namespace (its ui id) to read/write its data",
        )
    })
}

/// The durable-data namespace for App `ui_id`: follow its spec's `parent_app`
/// chain upward while each hop is **mutual** — the child names the parent AND
/// the parent's spec `app_ref`-embeds the child, by its ui id **or its name
/// slug** (all server-held facts, so a spec cannot claim its way into a foreign
/// namespace unilaterally). The topmost verified ancestor's id is the shared
/// namespace of the whole shell suite. Any broken/unverifiable hop (missing
/// parent, no reciprocal `app_ref`, a cycle, depth > 4) stops the walk —
/// failing closed to the last verified App.
pub(crate) async fn shared_app_namespace(
    store: &Store,
    ws: WorkspaceId,
    ui_id: catalerum_core::UiDefinitionId,
) -> String {
    let mut current = ui_id;
    let mut visited = vec![current];
    for _ in 0..4 {
        let Ok(def) = store.ui_definitions().get(ws, current).await else {
            break;
        };
        let Some(parent) = def.definition.parent_app.as_deref() else {
            break;
        };
        let Ok(parent_id) = parent.trim().parse::<catalerum_core::UiDefinitionId>() else {
            break;
        };
        if visited.contains(&parent_id) {
            break;
        }
        let Ok(parent_def) = store.ui_definitions().get(ws, parent_id).await else {
            break;
        };
        // The reciprocal `app_ref` may target the child by id or by name.
        let refs = catalerum_core::model_ui::collect_app_refs(&parent_def.definition);
        let by_id = refs.iter().any(|r| r == &current.to_string());
        let by_name = def
            .name
            .as_deref()
            .is_some_and(|n| refs.iter().any(|r| r == n));
        if !(by_id || by_name) {
            break;
        }
        current = parent_id;
        visited.push(current);
    }
    current.to_string()
}

/// `app_data_get` — read one value from an App's key/value store (SOUL §12/§29).
pub(crate) struct AppDataGetTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for AppDataGetTool {
    fn name(&self) -> &str {
        "app_data_get"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Read, "ui")
    }
    fn description(&self) -> &str {
        "Read one value from this App's durable key/value store by `key`, returning \
         `{ found, key, value }` (found=false when the key is unset). This is where an \
         emerged App persists its data model (a saved layout, a per-user tracker) so it \
         outlives the session — transient view state stays client-side. From an App \
         handler the namespace is this App automatically; elsewhere pass `app` (the App's \
         ui id) to read a specific App's data."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "key": { "type": "string", "description": "The key to read." },
                "app": { "type": "string", "description": "App namespace (its ui id). Ignored from an App handler (forced to that App); required otherwise." }
            },
            "required": ["key"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let app = app_data_namespace(&self.store, ctx, &args).await?;
        let key = required_str(&args, "key")?;
        match self.store.app_data().get(ws, &app, &key).await? {
            Some(e) => Ok(json!({
                "found": true,
                "app": e.app,
                "key": e.key,
                "value": e.value,
                "updated_at": e.updated_at.to_rfc3339(),
            })),
            None => Ok(json!({ "found": false, "app": app, "key": key })),
        }
    }
}

/// `app_data_set` — write one value into an App's key/value store (SOUL §12/§29).
pub(crate) struct AppDataSetTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for AppDataSetTool {
    fn name(&self) -> &str {
        "app_data_set"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "ui")
    }
    fn description(&self) -> &str {
        "Store one JSON `value` under `key` in this App's durable key/value store, \
         upserting (idempotent per key). Use for an App's persisted data model — a saved \
         dashboard layout, a habit tracker's per-user document (put the user in the key, \
         e.g. \"habits/<user>\"). Values are size-capped (~64 KiB) and the number of keys \
         per App is bounded; keep bulk data in files or an external database. From an App \
         handler the namespace is this App automatically; elsewhere pass `app`."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "key": { "type": "string", "description": "The key to write." },
                "value": { "description": "Any JSON value (object, array, string, number, bool, null)." },
                "app": { "type": "string", "description": "App namespace (its ui id). Ignored from an App handler; required otherwise." }
            },
            "required": ["key", "value"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let app = app_data_namespace(&self.store, ctx, &args).await?;
        let key = required_str(&args, "key")?;
        let value = args
            .get("value")
            .cloned()
            .ok_or_else(|| Error::invalid("`value` is required"))?;
        let e = self.store.app_data().set(ws, &app, &key, &value).await?;
        Ok(json!({
            "app": e.app,
            "key": e.key,
            "value": e.value,
            "updated_at": e.updated_at.to_rfc3339(),
        }))
    }
}

/// `app_data_list` — list an App's stored keys/values (SOUL §12/§29).
pub(crate) struct AppDataListTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for AppDataListTool {
    fn name(&self) -> &str {
        "app_data_list"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Read, "ui")
    }
    fn description(&self) -> &str {
        "List every `(key, value)` entry this App has stored in its durable key/value \
         store, key-ordered. Returns `{ app, count, entries }`. From an App handler the \
         namespace is this App automatically; elsewhere pass `app` (the App's ui id)."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "limit": { "type": "integer", "description": "Max entries (1-500, default 100).", "minimum": 1, "maximum": 500 },
                "app": { "type": "string", "description": "App namespace (its ui id). Ignored from an App handler; required otherwise." }
            }
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let app = app_data_namespace(&self.store, ctx, &args).await?;
        let limit = opt_clamped_u64(&args, "limit", 100, 500) as i64;
        let entries = self.store.app_data().list(ws, &app, limit).await?;
        let out: Vec<Json> = entries
            .into_iter()
            .map(|e| json!({ "key": e.key, "value": e.value, "updated_at": e.updated_at.to_rfc3339() }))
            .collect();
        Ok(json!({ "app": app, "count": out.len(), "entries": out }))
    }
}

/// `app_data_delete` — remove one key from an App's key/value store (SOUL §12/§29).
pub(crate) struct AppDataDeleteTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for AppDataDeleteTool {
    fn name(&self) -> &str {
        "app_data_delete"
    }
    fn required_capability(&self) -> Option<Capability> {
        // A write to the App's *own* private store — gated on `ui:write` (base
        // Member), not a protected `delete` scope, so it stays reachable from an App
        // handler/script rather than being confirm-required (SOUL §12/§19).
        cap(Action::Write, "ui")
    }
    fn description(&self) -> &str {
        "Delete one key from this App's durable key/value store. Idempotent — deleting \
         a missing key returns `{ deleted: false }`. From an App handler the namespace is \
         this App automatically; elsewhere pass `app` (the App's ui id)."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "key": { "type": "string", "description": "The key to delete." },
                "app": { "type": "string", "description": "App namespace (its ui id). Ignored from an App handler; required otherwise." }
            },
            "required": ["key"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let app = app_data_namespace(&self.store, ctx, &args).await?;
        let key = required_str(&args, "key")?;
        let deleted = self.store.app_data().delete(ws, &app, &key).await?;
        Ok(json!({ "deleted": deleted, "app": app, "key": key }))
    }
}
