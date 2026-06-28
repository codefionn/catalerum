//! Agent-profile-authoring tools (SOUL §19/§25).

use super::*;

/// The JSON-schema properties shared by `create_agent_profile`/`update_agent_profile`
/// (everything but the profile `name`, which each tool adds + requires). Mirrors the
/// REST `CreateAgentProfile`/`UpdateAgentProfile` bodies.
pub(crate) fn agent_profile_body_props() -> Json {
    json!({
        "model": {
            "type": "string",
            "description": "Model id the profile runs against (e.g. 'claude-...'); omit to use the workspace default."
        },
        "system_prompt": {
            "type": "string",
            "description": "The profile's system prompt; omit to use the default agent system prompt."
        },
        "tools": {
            "type": "array",
            "items": { "type": "string" },
            "description": "Tool names the profile may dispatch (a subset of the registry). Empty/omitted = all tools."
        },
        "skills": {
            "type": "array",
            "items": { "type": "string" },
            "description": "Skill names whose runbooks seed the profile's system prompt."
        },
        "subagents": {
            "type": "array",
            "items": { "type": "string" },
            "description": "Agent-profile names this profile may delegate to (each subagent runs under its own grant, enforced ⊆ this profile's authority at delegation time)."
        },
        "channels": {
            "type": "array",
            "items": { "type": "string" },
            "description": "Channel names this profile listens on: an inbound message on one routes to this profile (the channel→profile bridge)."
        },
        "grant_id": {
            "type": "string",
            "description": "UUID of the §19 grant that is this profile's authority; it must be ⊆ your own authority. Omit to run under bounded base-Member capabilities."
        },
        "guard": {
            "type": "object",
            "description": "Optional tool guard: a classifier gating EVERY tool call (and its output) on top of the grant — decides allow / deny / require-user-feedback. Use it to e.g. limit an MCP server to read-only or require approval for writes. Omit to leave the profile gated only by its capabilities.",
            "properties": {
                "script": {
                    "type": "string",
                    "description": "A Boa JS classifier: a function body receiving `input` = { phase:'call'|'output', tool:{name,description}, capability:{domain,action,read_only}, mcp:{server}|null, args, output }. Return 'allow' | 'deny' | 'ask' (or { decision, reason }). May call catalerum.callTool(name,args) and catalerum.classifyWithLlm({instruction, model?})."
                },
                "llm": {
                    "type": "object",
                    "description": "A declarative LLM classifier (used when there is no script, and as the default for classifyWithLlm).",
                    "properties": {
                        "instruction": { "type": "string", "description": "The judge's policy, e.g. 'Deny any write to production.'" },
                        "model": { "type": "string", "description": "Model to judge with; omit to use the profile's model." }
                    }
                },
                "object_labels": {
                    "type": "object",
                    "description": "Declarative allow/deny by the labels on the file a call touches (SOUL §9). Applied before the script/LLM to any call referencing an object (a key/path arg). e.g. only allow files labelled 'shared', or block anything labelled 'confidential'.",
                    "properties": {
                        "require_any": { "type": "array", "items": { "type": "string" }, "description": "If set, the object must carry at least one of these labels (unlabelled files are denied too)." },
                        "deny": { "type": "array", "items": { "type": "string" }, "description": "An object carrying any of these labels is blocked (wins over require_any)." }
                    }
                },
                "on_error": {
                    "type": "string",
                    "enum": ["deny", "allow", "ask"],
                    "description": "Fallback when the classifier errors or is unparseable. Default 'deny' (fail-closed)."
                }
            }
        }
    })
}

/// A compact summary of one agent profile for the list view (the name lists are
/// reduced to counts; the grant is reduced to a present/absent flag).
pub(crate) fn agent_profile_summary(p: &catalerum_core::model::AgentProfile) -> Json {
    json!({
        "name": p.name,
        "model": p.model,
        "tool_count": p.tools.len(),
        "skill_count": p.skills.len(),
        "subagents": p.subagents,
        "channels": p.channels,
        "has_grant": p.grant_id.is_some(),
    })
}

/// Resolve + authorize an optional `grant_id` for an agent-profile write tool: the
/// grant must exist in this workspace, and (SOUL §19 attenuation) its capabilities
/// must be ⊆ the **caller's own** authority (`ctx.capabilities`) — a profile can
/// never confer more than its creator holds. When the context carries no
/// capabilities (a trusted internal caller; dispatch enforcement is off), the
/// attenuation check is skipped, matching [`ToolRegistry::dispatch`]. The REST
/// path enforces the same invariant against the principal's role base-set (see
/// `routes::agent_profiles`).
pub(crate) async fn resolve_profile_grant(
    store: &Store,
    ctx: &ToolContext,
    ws: WorkspaceId,
    args: &Json,
) -> Result<Option<GrantId>> {
    let Some(raw) = opt_str_some(args, "grant_id") else {
        return Ok(None);
    };
    let id: GrantId = raw
        .parse()
        .map_err(|e| Error::invalid(format!("invalid grant_id: {e}")))?;
    let grant = store.grants().get(ws, id).await.map_err(|e| match e {
        StoreError::NotFound => Error::invalid("grant not found in this workspace"),
        other => other.into(),
    })?;
    if let Some(caps) = &ctx.capabilities {
        for cap in &grant.capabilities {
            if attenuate(caps, cap).is_err() {
                return Err(Error::unauthorized(
                    "profile grant exceeds your own authority",
                ));
            }
        }
    }
    Ok(Some(id))
}

/// Build the [`NewAgentProfile`] from a write tool's arguments (the `name` and the
/// already-resolved `grant_id` are passed in; the name lists are trimmed /
/// empties-dropped via `opt_str_vec`, and `model`/`system_prompt` are blank-as-absent).
pub(crate) fn agent_profile_spec(
    args: &Json,
    name: String,
    grant_id: Option<GrantId>,
) -> NewAgentProfile {
    NewAgentProfile {
        name,
        model: opt_str_some(args, "model"),
        system_prompt: opt_str_some(args, "system_prompt"),
        tools: opt_str_vec(args, "tools"),
        skills: opt_str_vec(args, "skills"),
        subagents: opt_str_vec(args, "subagents"),
        channels: opt_str_vec(args, "channels"),
        grant_id,
        guard: parse_guard_arg(args),
    }
}

/// Parse + normalize an optional `guard` object from a write tool's args (SOUL
/// §19): blank script/instruction fields become unset, and a guard with neither a
/// script nor a usable LLM classifier collapses to `None` (inert). Mirrors the
/// REST `clean_guard`, minus the async gateway model check — a bad model just
/// fails closed at judge time.
pub(crate) fn parse_guard_arg(args: &Json) -> Option<catalerum_core::model::ToolGuard> {
    let mut g: catalerum_core::model::ToolGuard =
        serde_json::from_value(args.get("guard")?.clone()).ok()?;
    if g.script.as_ref().is_some_and(|s| s.trim().is_empty()) {
        g.script = None;
    }
    if let Some(llm) = g.llm.as_mut() {
        llm.instruction = llm.instruction.trim().to_string();
        llm.model = llm
            .model
            .take()
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty());
    }
    if g.llm.as_ref().is_some_and(|l| l.instruction.is_empty()) {
        g.llm = None;
    }
    if let Some(policy) = g.object_labels.as_mut() {
        policy.require_any = normalize_label_list(std::mem::take(&mut policy.require_any));
        policy.deny = normalize_label_list(std::mem::take(&mut policy.deny));
    }
    if g.object_labels.as_ref().is_some_and(|p| p.is_empty()) {
        g.object_labels = None;
    }
    if g.script.is_none() && g.llm.is_none() && g.object_labels.is_none() {
        return None;
    }
    Some(g)
}

/// Trim, drop empties, and dedup a label list (order preserved).
pub(crate) fn normalize_label_list(items: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(items.len());
    for item in items {
        let t = item.trim();
        if !t.is_empty() && !out.iter().any(|x| x == t) {
            out.push(t.to_string());
        }
    }
    out
}

/// `list_agent_profiles` — the workspace's profiles, summarised by name.
pub(crate) struct ListAgentProfilesTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for ListAgentProfilesTool {
    fn name(&self) -> &str {
        "list_agent_profiles"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Read, "agent_profile")
    }
    fn description(&self) -> &str {
        "List the agent profiles in the user's workspace (name, model, tool/skill \
         counts, channels it listens on, and whether it carries a grant). An agent \
         profile is a durable, named scoped-agent configuration (SOUL §19/§25). Use \
         get_agent_profile for one profile's full definition."
    }
    fn parameters_schema(&self) -> Json {
        json!({ "type": "object", "properties": {} })
    }
    async fn invoke(&self, _args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let profiles = self.store.agent_profiles().list_by_workspace(ws).await?;
        let items: Vec<Json> = profiles.iter().map(agent_profile_summary).collect();
        Ok(json!({ "agent_profiles": items }))
    }
}

/// `get_agent_profile` — one profile's full definition by name.
pub(crate) struct GetAgentProfileTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for GetAgentProfileTool {
    fn name(&self) -> &str {
        "get_agent_profile"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Read, "agent_profile")
    }
    fn description(&self) -> &str {
        "Fetch one agent profile's full definition (model, system prompt, tools, \
         skills, subagents, channels, grant) by name. Errors if no profile with that \
         name exists."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "The agent profile's name." }
            },
            "required": ["name"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let name = required_str(&args, "name")?;
        let profile = self
            .store
            .agent_profiles()
            .get_by_name(ws, &name)
            .await?
            .ok_or_else(|| Error::invalid(format!("no agent profile named '{name}'")))?;
        Ok(serde_json::to_value(profile)?)
    }
}

/// `create_agent_profile` — author a new scoped-agent profile.
pub(crate) struct CreateAgentProfileTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for CreateAgentProfileTool {
    fn name(&self) -> &str {
        "create_agent_profile"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "agent_profile")
    }
    fn description(&self) -> &str {
        "Create a new agent profile (SOUL §19/§25) in the user's workspace: a durable, \
         named scoped-agent configuration that binds a model, an allowed tool/skill set, \
         the subagents it may delegate to, the channels it listens on, and the §19 grant \
         that is its authority. Provide a unique `name`; every other field is optional. \
         If `grant_id` is given it must be ⊆ your own authority (a profile can never \
         confer more than its creator holds). Errors if the name already exists — use \
         update_agent_profile to replace one."
    }
    fn parameters_schema(&self) -> Json {
        let mut props = agent_profile_body_props();
        if let Some(obj) = props.as_object_mut() {
            obj.insert(
                "name".to_string(),
                json!({ "type": "string", "description": "Unique agent-profile name (per workspace)." }),
            );
        }
        json!({ "type": "object", "properties": props, "required": ["name"] })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let name = required_str(&args, "name")?;
        let grant_id = resolve_profile_grant(&self.store, ctx, ws, &args).await?;
        let spec = agent_profile_spec(&args, name, grant_id);
        let profile = self.store.agent_profiles().create(ws, &spec).await?;
        Ok(serde_json::to_value(profile)?)
    }
}

/// `update_agent_profile` — create-or-replace a profile by name.
pub(crate) struct UpdateAgentProfileTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for UpdateAgentProfileTool {
    fn name(&self) -> &str {
        "update_agent_profile"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "agent_profile")
    }
    fn description(&self) -> &str {
        "Create or replace an agent profile by name (full replacement of its \
         definition; the stable id is kept if it already exists). Same fields and §19 \
         grant attenuation as create_agent_profile. Use this to edit an existing \
         profile: get_agent_profile, change the parts you want, then \
         update_agent_profile with the full new definition (omitted fields reset to \
         their defaults — it is a replacement, not a merge)."
    }
    fn parameters_schema(&self) -> Json {
        let mut props = agent_profile_body_props();
        if let Some(obj) = props.as_object_mut() {
            obj.insert(
                "name".to_string(),
                json!({ "type": "string", "description": "Name of the agent profile to create or replace." }),
            );
        }
        json!({ "type": "object", "properties": props, "required": ["name"] })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let name = required_str(&args, "name")?;
        let grant_id = resolve_profile_grant(&self.store, ctx, ws, &args).await?;
        let spec = agent_profile_spec(&args, name, grant_id);
        let profile = self
            .store
            .agent_profiles()
            .upsert_by_name(ws, &spec)
            .await?;
        Ok(serde_json::to_value(profile)?)
    }
}

/// `delete_agent_profile` — remove a profile by name.
pub(crate) struct DeleteAgentProfileTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for DeleteAgentProfileTool {
    fn name(&self) -> &str {
        "delete_agent_profile"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "agent_profile")
    }
    fn description(&self) -> &str {
        "Permanently delete an agent profile by name. Errors if no profile with that \
         name exists. This cannot be undone."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "The agent profile's name." }
            },
            "required": ["name"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let name = required_str(&args, "name")?;
        let profile = self
            .store
            .agent_profiles()
            .get_by_name(ws, &name)
            .await?
            .ok_or_else(|| Error::invalid(format!("no agent profile named '{name}'")))?;
        self.store.agent_profiles().delete(ws, profile.id).await?;
        Ok(json!({ "deleted": name }))
    }
}
