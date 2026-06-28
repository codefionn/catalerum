//! Automation-authoring tools + trigger tools (SOUL §11).

use super::*;

/// `fire_trigger` — emit a **named signal** that runs every enabled automation
/// headed by a matching `{ "kind": "trigger", "name": … }` trigger (SOUL §11/§12).
///
/// This is the on-demand bridge for an **emerged UI**: a button/handler (declarative
/// or a Boa script via `catalerum.callTool`) calls this with a signal `name` and an
/// optional `payload`, and the runtime enqueues a durable `run_automation` job for
/// each matching automation — the same dispatch path the webhook / Kanban sources
/// use. It is equally reachable from chat and from an automation code node. Gated on
/// `automation:write` (base-Member, so a Viewer is denied and — being ≤ base-Member —
/// it stays script-callable and fits the default UI handler allow-list). The caller's
/// role gates *triggering* only; each automation then runs under its own §19 authority.
pub(crate) struct FireTriggerTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for FireTriggerTool {
    fn name(&self) -> &str {
        "fire_trigger"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "automation")
    }
    fn description(&self) -> &str {
        "Fire a named automation signal on demand. Runs every enabled automation \
         whose trigger is { kind: \"trigger\", name: <name> }. Use this to let an \
         emerged-UI button (or chat) kick off a backend workflow. An optional \
         `payload` object is carried on the run for the automation to read; it does \
         not affect which automations match. Returns how many matched and their job ids."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "The signal name to fire; matched exactly (case-sensitive) against `trigger` triggers."
                },
                "payload": {
                    "type": "object",
                    "description": "Optional context carried on the run's trigger event (not used for matching)."
                }
            },
            "required": ["name"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let name = required_str(&args, "name")?;
        let payload = args.get("payload").filter(|v| !v.is_null()).cloned();
        let event = catalerum_automation::TriggerEvent::Trigger { name, payload };
        let jobs = catalerum_ingest::dispatch_trigger_event(&self.store, ws, &event)
            .await
            .map_err(|e| Error::provider(format!("dispatching trigger automations: {e}")))?;
        Ok(json!({ "matched": jobs.len(), "jobs": jobs }))
    }
}

/// `trigger_link` — mint a **public, signed** URL an external caller can `POST` to
/// fire one named automation signal without a login (SOUL §11/§12/§18).
///
/// The public twin of [`FireTriggerTool`]: use it to hand a CI job / device / third
/// party a link that fires a specific `trigger` signal (e.g. give your build server a
/// URL for the `deploy-done` signal). The link points at `POST /triggers/fire/{token}`;
/// the token is an HMAC-signed claim naming exactly one workspace + signal name +
/// expiry, so it grants firing of that **one** signal for a **short** window and
/// nothing else (§18/§19) — each automation it fans out to still runs under its own
/// authority. Gated on `automation:write` (like `fire_trigger` / the webhook route).
pub(crate) struct TriggerLinkTool {
    pub(crate) signer: TriggerSigner,
    /// The API's public base URL (no trailing slash) links are rendered against.
    pub(crate) base_url: String,
}

#[async_trait]
impl Tool for TriggerLinkTool {
    fn name(&self) -> &str {
        "trigger_link"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "automation")
    }
    fn description(&self) -> &str {
        "Generate a short-lived, shareable URL that fires a named automation signal \
         when POSTed to — no login needed. Use this to let an external service (a CI \
         job, a device, a third-party webhook) run an automation on demand: give the \
         signal `name` an automation's { kind: \"trigger\", name } trigger listens for. \
         The link expires (default 1 hour). Returns the `url` and its `expires_at`. \
         (For firing from chat or an emerged UI, call `fire_trigger` directly instead.)"
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "The signal name the link fires; matched exactly against `trigger` triggers." },
                "ttl_secs": { "type": "integer", "description": "How long the link stays valid, in seconds. Default 3600 (1 hour); clamped to [60, 604800] (max 7 days)." }
            },
            "required": ["name"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let name = required_str(&args, "name")?;
        // Trigger links share the download-link TTL policy (1 h default, [60 s, 7 d]).
        let ttl = opt_clamped_u64(
            &args,
            "ttl_secs",
            DEFAULT_DOWNLOAD_TTL_SECS,
            MAX_DOWNLOAD_TTL_SECS,
        )
        .max(MIN_DOWNLOAD_TTL_SECS);
        let exp = chrono::Utc::now().timestamp() + ttl as i64;
        let claims = TriggerClaims {
            workspace_id: ws,
            name: name.clone(),
            exp,
        };
        let token = self.signer.mint(&claims);
        let url = format!("{}/triggers/fire/{token}", self.base_url);
        let expires_at = chrono::DateTime::<chrono::Utc>::from_timestamp(exp, 0)
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_default();
        Ok(json!({ "url": url, "name": name, "expires_at": expires_at }))
    }
}

/// Register `trigger_link` (SOUL §11/§12). Called from [`AppState`](crate::state::AppState)
/// (unconditionally — no storage dependency), threading the [`TriggerSigner`] + API
/// base URL so the tool mints links the public `POST /triggers/fire/{token}` route
/// verifies.
pub(crate) fn register_trigger_link_tool(
    registry: &mut ToolRegistry,
    signer: TriggerSigner,
    base_url: String,
) {
    registry.register(Arc::new(TriggerLinkTool { signer, base_url }));
}

/// Validate + compile a create/replace into the stored `triggers` column, mirroring
/// the `/automations` REST path: a **graph** spec (its `spec` carries a `"graph"`)
/// is parsed + validated and its Trigger nodes are compiled into dispatch triggers;
/// a **linear** spec is validated via [`AutomationSpec`](catalerum_automation::AutomationSpec).
/// Returns the triggers to persist, or an `invalid`-kind error (a `400` equivalent).
pub(crate) fn compile_automation_triggers(
    spec: Option<&Json>,
    triggers: Vec<Json>,
    condition: Option<&Json>,
    actions: &[Json],
) -> Result<Vec<Json>> {
    use catalerum_automation::graph::Graph;
    if let Some(parsed) = Graph::from_spec(spec) {
        let graph = parsed.map_err(|e| Error::invalid(format!("invalid automation graph: {e}")))?;
        graph
            .validate()
            .map_err(|e| Error::invalid(format!("invalid automation graph: {e}")))?;
        Ok(graph
            .trigger_specs()
            .iter()
            .map(|t| serde_json::to_value(t).unwrap_or(Json::Null))
            .collect())
    } else {
        catalerum_automation::AutomationSpec::from_json(&triggers, condition, actions)
            .map_err(|e| Error::invalid(format!("invalid automation: {e}")))?;
        Ok(triggers)
    }
}

/// Pull an optional array argument at `key` as raw JSON values (absent → empty).
pub(crate) fn opt_json_array(args: &Json, key: &str) -> Vec<Json> {
    args.get(key)
        .and_then(Json::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Pull an optional JSON object/value argument at `key`, treating `null` as absent.
pub(crate) fn opt_json_value(args: &Json, key: &str) -> Option<Json> {
    match args.get(key) {
        Some(Json::Null) | None => None,
        Some(v) => Some(v.clone()),
    }
}

/// Non-fatal graph diagnostics for a create/edit response (SOUL §11): the graph's
/// [`Graph::warnings`](catalerum_automation::graph::Graph::warnings) — a node no
/// trigger reaches, a trigger wired to nothing, a dead condition branch. Empty for a
/// linear/non-graph spec (nothing to warn about) or an unparseable graph (the caller
/// has already rejected that shape via [`compile_automation_triggers`]).
pub(crate) fn graph_warnings(spec: Option<&Json>) -> Vec<String> {
    match catalerum_automation::graph::Graph::from_spec(spec) {
        Some(Ok(graph)) => graph.warnings(),
        _ => Vec::new(),
    }
}

/// Serialize a just-persisted automation for a create/edit/update tool response,
/// attaching any non-fatal validation `warnings` (an empty array when clean) so the
/// author sees probable mistakes the save didn't block — the graph still saved and
/// runs, but a warned node never fires. Mirrors the hard-error path (a truly invalid
/// graph is rejected up front by [`compile_automation_triggers`]).
pub(crate) fn automation_response(
    automation: catalerum_core::Automation,
    warnings: Vec<String>,
) -> Result<Json> {
    let mut value = serde_json::to_value(automation)?;
    if let Some(obj) = value.as_object_mut() {
        obj.insert("warnings".to_string(), json!(warnings));
    }
    Ok(value)
}

/// The shared JSON-schema fragment for a create/replace body (triggers/condition/
/// actions/spec). A linear automation supplies `triggers` + `actions`; a node-graph
/// automation supplies `spec.graph` (and leaves the linear fields empty — the
/// dispatch triggers are compiled from the graph). Use `search_automation_node_types` /
/// `list_automation_node_types` to discover the trigger/action shapes.
pub(crate) fn automation_body_props() -> Json {
    json!({
        "enabled": { "type": "boolean", "description": "Whether the automation is active. Defaults to true." },
        "triggers": {
            "type": "array",
            "items": { "type": "object" },
            "description": "Linear automation: typed trigger specs, e.g. [{\"kind\":\"schedule\",\"cron\":\"0 9 * * *\"}]. Leave empty for a graph automation (compiled from spec.graph)."
        },
        "condition": { "description": "Optional condition predicate (kept as JSON). Omit for none." },
        "actions": {
            "type": "array",
            "items": { "type": "object" },
            "description": "Linear automation: ordered typed action specs, e.g. [{\"kind\":\"create_note\",\"title\":\"x\"}]. Leave empty for a graph automation."
        },
        "spec": {
            "type": "object",
            "description": "Optional authoring spec, round-tripped verbatim. A node-graph automation puts its graph under spec.graph: {\"graph\":{\"nodes\":[...],\"edges\":[...]}}."
        }
    })
}

/// `list_automations` — the workspace's automations (compact view).
pub(crate) struct ListAutomationsTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for ListAutomationsTool {
    fn name(&self) -> &str {
        "list_automations"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Read, "automation")
    }
    fn description(&self) -> &str {
        "List the automations in the user's workspace (name, enabled state, trigger \
         kinds, action count, and whether it is a node graph). Use get_automation for \
         the full definition of one."
    }
    fn parameters_schema(&self) -> Json {
        json!({ "type": "object", "properties": {} })
    }
    async fn invoke(&self, _args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let automations = self.store.automations().list_by_workspace(ws).await?;
        let items: Vec<Json> = automations.iter().map(automation_summary).collect();
        Ok(json!({ "automations": items }))
    }
}

/// A compact summary of one automation for the list view.
pub(crate) fn automation_summary(a: &catalerum_core::Automation) -> Json {
    let trigger_kinds: Vec<&str> = a
        .triggers
        .iter()
        .filter_map(|t| t.get("kind").and_then(Json::as_str))
        .collect();
    let is_graph = a.spec.as_ref().and_then(|s| s.get("graph")).is_some();
    json!({
        "name": a.name,
        "enabled": a.enabled,
        "trigger_kinds": trigger_kinds,
        "action_count": a.actions.len(),
        "is_graph": is_graph,
    })
}

/// `get_automation` — one automation's full definition by name.
pub(crate) struct GetAutomationTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for GetAutomationTool {
    fn name(&self) -> &str {
        "get_automation"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Read, "automation")
    }
    fn description(&self) -> &str {
        "Fetch one automation's full definition (triggers, condition, actions, and \
         spec/graph) by name. Errors if no automation with that name exists."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "The automation's name." }
            },
            "required": ["name"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let name = required_str(&args, "name")?;
        let automation = self
            .store
            .automations()
            .get_by_name(ws, &name)
            .await?
            .ok_or_else(|| Error::invalid(format!("no automation named '{name}'")))?;
        Ok(serde_json::to_value(automation)?)
    }
}

/// `create_automation` — author a new automation (linear or node graph).
pub(crate) struct CreateAutomationTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for CreateAutomationTool {
    fn name(&self) -> &str {
        "create_automation"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "automation")
    }
    fn description(&self) -> &str {
        "Create a new automation (SOUL §11) in the user's workspace. Provide a unique \
         name plus either linear triggers+actions, or a node graph under spec.graph. \
         The spec is validated before saving (unknown kinds, missing fields, invalid \
         cron, or a cyclic/triggerless graph are rejected). Errors if the name already \
         exists. On success returns the saved automation plus a `warnings` array of \
         non-fatal issues that did NOT block the save but probably indicate a mistake \
         (e.g. a node not connected to any trigger so it never runs, a trigger wired \
         to nothing, or a condition branch left unwired) — review it and fix or \
         confirm. Discover node types with list_automation_node_types / \
         search_automation_node_types; dry-run a draft with test_automation first."
    }
    fn parameters_schema(&self) -> Json {
        let mut props = automation_body_props();
        if let Some(obj) = props.as_object_mut() {
            obj.insert(
                "name".to_string(),
                json!({ "type": "string", "description": "Unique automation name (per workspace)." }),
            );
        }
        json!({ "type": "object", "properties": props, "required": ["name"] })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let name = required_str(&args, "name")?;
        let enabled = args.get("enabled").and_then(Json::as_bool).unwrap_or(true);
        let req_triggers = opt_json_array(&args, "triggers");
        let condition = opt_json_value(&args, "condition");
        let actions = opt_json_array(&args, "actions");
        let mut spec = opt_json_value(&args, "spec");
        let triggers =
            compile_automation_triggers(spec.as_ref(), req_triggers, condition.as_ref(), &actions)?;
        // Agents rarely author canvas positions — lay out any origin-defaulted
        // graph nodes so the visual editor doesn't open them stacked (SOUL §11).
        if let Some(s) = spec.as_mut() {
            catalerum_automation::apply_auto_layout(s);
        }
        // A collect trigger must reference an EXISTING connection (SOUL §10/§28) —
        // a placeholder id would save fine and then fail forever at poll time.
        crate::connection_status::validate_collect_connections(&self.store, ws, &triggers)
            .await
            .map_err(Error::invalid)?;
        // Non-fatal graph diagnostics (a disconnected node, an unwired trigger, …)
        // surfaced on the response — these don't block the save (SOUL §11).
        let warnings = graph_warnings(spec.as_ref());
        let new = catalerum_store::NewAutomation {
            name,
            enabled,
            triggers,
            condition,
            actions,
            spec,
            grant_id: None,
        };
        let automation = self.store.automations().create(ws, &new).await?;
        automation_response(automation, warnings)
    }
}

/// `update_automation` — create-or-replace an automation by name.
pub(crate) struct UpdateAutomationTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for UpdateAutomationTool {
    fn name(&self) -> &str {
        "update_automation"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "automation")
    }
    fn description(&self) -> &str {
        "Create or replace an automation by name (full replacement of its definition; \
         the stable id is kept if it already exists). Same validation as \
         create_automation, and the same `warnings` array of non-fatal issues on the \
         response (e.g. a node not connected to any trigger). Use this to edit an \
         existing automation: get_automation, change the parts you want, then \
         update_automation with the full new definition."
    }
    fn parameters_schema(&self) -> Json {
        let mut props = automation_body_props();
        if let Some(obj) = props.as_object_mut() {
            obj.insert(
                "name".to_string(),
                json!({ "type": "string", "description": "Name of the automation to create or replace." }),
            );
        }
        json!({ "type": "object", "properties": props, "required": ["name"] })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let name = required_str(&args, "name")?;
        let enabled = args.get("enabled").and_then(Json::as_bool).unwrap_or(true);
        let req_triggers = opt_json_array(&args, "triggers");
        let condition = opt_json_value(&args, "condition");
        let actions = opt_json_array(&args, "actions");
        let mut spec = opt_json_value(&args, "spec");
        let triggers =
            compile_automation_triggers(spec.as_ref(), req_triggers, condition.as_ref(), &actions)?;
        // As in create_automation: auto-place origin-defaulted graph nodes.
        if let Some(s) = spec.as_mut() {
            catalerum_automation::apply_auto_layout(s);
        }
        // Same collect-connection existence guard as create_automation (SOUL §10/§28).
        crate::connection_status::validate_collect_connections(&self.store, ws, &triggers)
            .await
            .map_err(Error::invalid)?;
        // Same non-fatal graph diagnostics as create_automation (SOUL §11).
        let warnings = graph_warnings(spec.as_ref());
        let new = catalerum_store::NewAutomation {
            name,
            enabled,
            triggers,
            condition,
            actions,
            spec,
            grant_id: None,
        };
        let automation = self.store.automations().upsert_by_name(ws, &new).await?;
        automation_response(automation, warnings)
    }
}

/// `edit_automation` — **partial** update of an existing automation by name.
///
/// The automation twin of [`EditSkillTool`](super::skills::EditSkillTool) (SOUL §11/§23):
/// only the fields the caller passes change; an omitted field keeps its stored
/// value. This is the safe editor — unlike [`UpdateAutomationTool`] (a *full*
/// replacement, so calling it without `spec` would wipe a node graph, and its
/// `grant_id: None` drops the grant), `edit_automation` preserves the untouched
/// parts (the graph, the condition, and the §19 grant the automation runs under).
/// Read-merge-upsert, not a transaction (a concurrent delete turns the upsert into
/// a create that still lands the caller's intent). Same validation as
/// `create_automation`; errors on an unknown name.
pub(crate) struct EditAutomationTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for EditAutomationTool {
    fn name(&self) -> &str {
        "edit_automation"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "automation")
    }
    fn description(&self) -> &str {
        "Partially edit an existing automation by name — only the fields you pass \
         change; every field you omit keeps its stored value. This is the safe way \
         to tweak one part of an automation: unlike update_automation (which REPLACES \
         the whole definition, so calling it without `spec` would wipe a node graph), \
         edit_automation preserves the rest, including the graph and the grant the \
         automation runs under. Pass any of enabled / triggers / condition / actions / \
         spec. For a node-graph automation, edit by passing the full new `spec` (its \
         dispatch triggers are recompiled from spec.graph); for a linear automation, \
         pass triggers and/or actions. `\"condition\": null` clears the condition; \
         `\"spec\": null` drops the graph (making it a linear automation). To only \
         pause or resume, prefer set_automation_enabled. Errors on an unknown name — \
         use create_automation to make a new one. On success returns the edited \
         automation plus a `warnings` array of non-fatal issues (e.g. a node not \
         connected to any trigger, so it never runs) — review it and fix or confirm."
    }
    fn parameters_schema(&self) -> Json {
        // A dedicated schema (not the shared `automation_body_props`) so each field
        // carries this tool's keep-on-omit semantics for the model.
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Name of the automation to edit (identifies it; not changeable). Errors if no such automation exists." },
                "enabled": { "type": "boolean", "description": "New active state (true = resume, false = pause). Omit to keep current — or use set_automation_enabled to only toggle." },
                "triggers": {
                    "type": "array",
                    "items": { "type": "object" },
                    "description": "Linear automation: replace the trigger specs. Omit to keep current. Ignored for a graph automation (its triggers are recompiled from spec.graph)."
                },
                "condition": { "description": "Replace the condition predicate (kept as JSON). Omit to keep current; pass null to clear it." },
                "actions": {
                    "type": "array",
                    "items": { "type": "object" },
                    "description": "Linear automation: replace the ordered action specs. Omit to keep current."
                },
                "spec": {
                    "type": "object",
                    "description": "Replace the authoring spec, e.g. a node graph under spec.graph. Omit to keep current; pass null to drop the graph (making it a linear automation)."
                }
            },
            "required": ["name"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let name = required_str(&args, "name")?;
        let existing = self
            .store
            .automations()
            .get_by_name(ws, &name)
            .await?
            .ok_or_else(|| {
                Error::invalid(format!(
                    "no automation named '{name}' — use create_automation to make a new one"
                ))
            })?;
        // Merge: a present key replaces the stored value, an absent key keeps it. A
        // present-but-empty array still replaces (matching edit_skill); for the two
        // nullable fields an explicit `null` clears (condition / graph spec).
        let enabled = args
            .get("enabled")
            .and_then(Json::as_bool)
            .unwrap_or(existing.enabled);
        let req_triggers = match args.get("triggers") {
            Some(_) => opt_json_array(&args, "triggers"),
            None => existing.triggers.clone(),
        };
        let condition = match args.get("condition") {
            None => existing.condition.clone(),
            Some(Json::Null) => None,
            Some(v) => Some(v.clone()),
        };
        let actions = match args.get("actions") {
            Some(_) => opt_json_array(&args, "actions"),
            None => existing.actions.clone(),
        };
        let mut spec = match args.get("spec") {
            None => existing.spec.clone(),
            Some(Json::Null) => None,
            Some(v) => Some(v.clone()),
        };
        // Recompile the dispatch triggers from the merged body (a graph spec ignores
        // the linear triggers and recompiles from spec.graph, so a graph automation's
        // `triggers` stay in sync even when only its spec changed).
        let triggers =
            compile_automation_triggers(spec.as_ref(), req_triggers, condition.as_ref(), &actions)?;
        // As in create/update_automation: auto-place any origin-defaulted graph nodes.
        if let Some(s) = spec.as_mut() {
            catalerum_automation::apply_auto_layout(s);
        }
        // Same collect-connection existence guard as create/update (SOUL §10/§28).
        crate::connection_status::validate_collect_connections(&self.store, ws, &triggers)
            .await
            .map_err(Error::invalid)?;
        // Same non-fatal graph diagnostics as create/update (SOUL §11).
        let warnings = graph_warnings(spec.as_ref());
        let new = catalerum_store::NewAutomation {
            name,
            enabled,
            triggers,
            condition,
            actions,
            spec,
            // Preserve the grant the automation already runs under (full-replacement
            // update_automation drops it; a partial edit must not).
            grant_id: existing.grant_id,
        };
        let automation = self.store.automations().upsert_by_name(ws, &new).await?;
        automation_response(automation, warnings)
    }
}

/// `set_automation_enabled` — pause/resume without rewriting the definition.
pub(crate) struct SetAutomationEnabledTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for SetAutomationEnabledTool {
    fn name(&self) -> &str {
        "set_automation_enabled"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "automation")
    }
    fn description(&self) -> &str {
        "Enable (resume) or disable (pause) an automation by name without changing its \
         definition. A disabled automation stops firing until re-enabled."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "The automation's name." },
                "enabled": { "type": "boolean", "description": "true to resume, false to pause." }
            },
            "required": ["name", "enabled"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let name = required_str(&args, "name")?;
        let enabled = args
            .get("enabled")
            .and_then(Json::as_bool)
            .ok_or_else(|| Error::invalid("`enabled` (boolean) is required"))?;
        let automation = self
            .store
            .automations()
            .get_by_name(ws, &name)
            .await?
            .ok_or_else(|| Error::invalid(format!("no automation named '{name}'")))?;
        let updated = self
            .store
            .automations()
            .set_enabled(ws, automation.id, enabled)
            .await?;
        Ok(serde_json::to_value(updated)?)
    }
}

/// `delete_automation` — remove an automation by name.
pub(crate) struct DeleteAutomationTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for DeleteAutomationTool {
    fn name(&self) -> &str {
        "delete_automation"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "automation")
    }
    fn description(&self) -> &str {
        "Permanently delete an automation by name. Errors if no automation with that \
         name exists. This cannot be undone — prefer set_automation_enabled(false) to \
         pause one you may want back."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "The automation's name." }
            },
            "required": ["name"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let name = required_str(&args, "name")?;
        let automation = self
            .store
            .automations()
            .get_by_name(ws, &name)
            .await?
            .ok_or_else(|| Error::invalid(format!("no automation named '{name}'")))?;
        self.store.automations().delete(ws, automation.id).await?;
        Ok(json!({ "deleted": name }))
    }
}

/// `test_automation` — dry-run validation of a draft spec **without persisting**.
pub(crate) struct TestAutomationTool;

#[async_trait]
impl Tool for TestAutomationTool {
    fn name(&self) -> &str {
        "test_automation"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Read, "automation")
    }
    fn description(&self) -> &str {
        "Dry-run validate a draft automation spec WITHOUT saving it (SOUL §11). Accepts \
         the same fields as create_automation (triggers/condition/actions or spec.graph) \
         and reports whether it is valid, plus — for a node graph — the compiled \
         dispatch triggers, node/edge counts, topological run order, and a `warnings` \
         array of non-fatal issues (a node not connected to any trigger, a trigger \
         wired to nothing, a dead condition branch), or the first validation error. \
         Use this to iterate on a draft before create_automation."
    }
    fn parameters_schema(&self) -> Json {
        json!({ "type": "object", "properties": automation_body_props() })
    }
    async fn invoke(&self, args: Json, _ctx: &ToolContext) -> Result<Json> {
        use catalerum_automation::graph::Graph;
        let spec = opt_json_value(&args, "spec");
        let req_triggers = opt_json_array(&args, "triggers");
        let condition = opt_json_value(&args, "condition");
        let actions = opt_json_array(&args, "actions");

        // A node-graph draft: parse + validate the graph and report its shape.
        if let Some(parsed) = Graph::from_spec(spec.as_ref()) {
            let graph = match parsed {
                Ok(g) => g,
                Err(e) => return Ok(json!({ "valid": false, "kind": "graph", "error": e })),
            };
            if let Err(e) = graph.validate() {
                return Ok(json!({ "valid": false, "kind": "graph", "error": e }));
            }
            let compiled: Vec<Json> = graph
                .trigger_specs()
                .iter()
                .map(|t| serde_json::to_value(t).unwrap_or(Json::Null))
                .collect();
            let order = graph.topo_order().unwrap_or_default();
            return Ok(json!({
                "valid": true,
                "kind": "graph",
                "node_count": graph.nodes.len(),
                "edge_count": graph.edges.len(),
                "compiled_triggers": compiled,
                "run_order": order,
                // Non-fatal diagnostics (disconnected node, unwired trigger, dead
                // condition branch) — the same warnings create/edit surface.
                "warnings": graph.warnings(),
            }));
        }

        // A linear draft: validate the typed spec.
        match catalerum_automation::AutomationSpec::from_json(
            &req_triggers,
            condition.as_ref(),
            &actions,
        ) {
            Ok(spec) => {
                let trigger_kinds: Vec<&str> = spec.triggers.iter().map(|t| t.kind()).collect();
                Ok(json!({
                    "valid": true,
                    "kind": "linear",
                    "trigger_kinds": trigger_kinds,
                    "action_count": spec.actions.len(),
                    // A linear automation is connected by construction — no graph
                    // warnings — but report the field for a uniform response shape.
                    "warnings": Vec::<String>::new(),
                }))
            }
            Err(e) => Ok(json!({ "valid": false, "kind": "linear", "error": e.to_string() })),
        }
    }
}

/// `run_automation` — fire an existing automation once now (a real run on the worker).
pub(crate) struct RunAutomationTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for RunAutomationTool {
    fn name(&self) -> &str {
        "run_automation"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "automation")
    }
    fn description(&self) -> &str {
        "Manually run an existing automation once, right now (SOUL §11) — enqueues a \
         durable run that the worker executes out-of-band (its actions have real \
         effects). Returns the enqueued job id; the automation must be enabled to run. \
         Use this to test a saved automation end-to-end, then inspect its runs."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Name of the automation to run now." }
            },
            "required": ["name"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let name = required_str(&args, "name")?;
        let automation = self
            .store
            .automations()
            .get_by_name(ws, &name)
            .await?
            .ok_or_else(|| Error::invalid(format!("no automation named '{name}'")))?;
        if !automation.enabled {
            return Err(Error::invalid(format!(
                "automation '{name}' is disabled; enable it first to run it"
            )));
        }
        let job = catalerum_ingest::enqueue_run_automation(&self.store, ws, automation.id, None)
            .await
            .map_err(|e| Error::provider(format!("failed to enqueue run: {e}")))?;
        Ok(json!({ "enqueued": true, "automation": name, "job_id": job.to_string() }))
    }
}

/// `list_automation_node_types` — browse the node-type catalog (the building blocks
/// of an automation graph). Ungated, like `search_automation_node_types`: global
/// documentation, no workspace data.
pub(crate) struct ListAutomationNodeTypesTool;

#[async_trait]
impl Tool for ListAutomationNodeTypesTool {
    fn name(&self) -> &str {
        "list_automation_node_types"
    }
    fn description(&self) -> &str {
        "List the automation node types you can place in an automation graph — the \
         triggers (what fires it), actions (what it does), and the code/condition \
         nodes. Returns each type's id, node_kind, kind, title and one-line summary. \
         Optionally filter by node_kind ('trigger' | 'action' | 'code' | 'condition'). \
         Then call get_automation_node_type for one type's full params + example, or \
         search_automation_node_types to find a type by intent — before create_automation."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "node_kind": {
                    "type": "string",
                    "description": "Optional filter: only this node kind.",
                    "enum": ["trigger", "action", "code", "condition"]
                }
            }
        })
    }
    async fn invoke(&self, args: Json, _ctx: &ToolContext) -> Result<Json> {
        let filter = opt_str_some(&args, "node_kind");
        let node_types: Vec<Json> = catalerum_automation::catalog()
            .iter()
            .filter(|d| filter.as_deref().is_none_or(|k| d.node_kind == k))
            .map(|d| {
                json!({
                    "id": d.id,
                    "node_kind": d.node_kind,
                    "kind": d.kind,
                    "title": d.title,
                    "summary": d.summary,
                })
            })
            .collect();
        Ok(json!({ "node_types": node_types }))
    }
}

/// `get_automation_node_type` — read one node type's full documentation by id.
/// Ungated, like `list_automation_node_types`.
pub(crate) struct GetAutomationNodeTypeTool;

#[async_trait]
impl Tool for GetAutomationNodeTypeTool {
    fn name(&self) -> &str {
        "get_automation_node_type"
    }
    fn description(&self) -> &str {
        "Get the full documentation for one automation node type by id (e.g. \
         'trigger.schedule', 'action.create_note', 'code', 'condition'): its \
         description, typed params (name/type/required/description), and a \
         ready-to-paste example graph node. Use this to learn a node's exact fields \
         before authoring it with create_automation / update_automation. Discover ids \
         with list_automation_node_types or search_automation_node_types."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Node-type id: 'trigger.<kind>', 'action.<kind>', 'code', or 'condition'."
                }
            },
            "required": ["id"]
        })
    }
    async fn invoke(&self, args: Json, _ctx: &ToolContext) -> Result<Json> {
        let id = required_str(&args, "id")?;
        let doc = catalerum_automation::catalog::get(&id).ok_or_else(|| {
            Error::invalid(format!(
                "no automation node type '{id}'; use list_automation_node_types to see the available ids"
            ))
        })?;
        Ok(serde_json::to_value(doc)?)
    }
}

/// Register the automation-authoring tools (SOUL §11). Always available — thin
/// `Store` clients gated on `automation:read`/`automation:write` (§19), plus the
/// ungated node-type catalog readers (`list_automation_node_types` /
/// `get_automation_node_type` / `search_automation_node_types` — global documentation).
pub(crate) fn register_automation_tools(registry: &mut ToolRegistry, store: &Store) {
    registry.register(Arc::new(ListAutomationNodeTypesTool));
    registry.register(Arc::new(GetAutomationNodeTypeTool));
    registry.register(Arc::new(ListAutomationsTool {
        store: store.clone(),
    }));
    registry.register(Arc::new(GetAutomationTool {
        store: store.clone(),
    }));
    registry.register(Arc::new(CreateAutomationTool {
        store: store.clone(),
    }));
    registry.register(Arc::new(UpdateAutomationTool {
        store: store.clone(),
    }));
    registry.register(Arc::new(EditAutomationTool {
        store: store.clone(),
    }));
    registry.register(Arc::new(SetAutomationEnabledTool {
        store: store.clone(),
    }));
    registry.register(Arc::new(DeleteAutomationTool {
        store: store.clone(),
    }));
    registry.register(Arc::new(TestAutomationTool));
    registry.register(Arc::new(RunAutomationTool {
        store: store.clone(),
    }));
}

// ===========================================================================
// Agent-profile authoring (SOUL §19/§25)
// ===========================================================================
