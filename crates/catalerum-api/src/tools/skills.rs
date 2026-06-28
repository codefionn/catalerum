//! Skill tools (SOUL §23).

use super::*;

/// `use_skill` — invoke a named skill (SOUL §23): return its markdown runbook
/// (and allowed tools) for the model to follow. Capability-gated **per skill**
/// (`skill:use@<name>`, §19): the check is done in `invoke` (not via the
/// dispatch-level `required_capability`, which would be whole-domain and so
/// reject a narrowly-scoped `skill:use@<one>` grant). Running a skill's `code`
/// via the Executor + enforcing its restricted tool set layer on later.
pub(crate) struct UseSkillTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for UseSkillTool {
    fn name(&self) -> &str {
        "use_skill"
    }
    fn description(&self) -> &str {
        "Invoke a saved skill by name. Returns its instructions (a runbook to \
         follow) and the tools it is meant to use; those tools are automatically \
         advertised for the next step. Use `list_skills` to discover skills."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": { "name": { "type": "string", "description": "Skill name to invoke." } },
            "required": ["name"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let name = required_str(&args, "name")?;
        let skill = self
            .store
            .skills()
            .get_by_name(ws, &name)
            .await?
            .ok_or_else(|| Error::invalid(format!("unknown skill `{name}`")))?;
        // Per-skill capability gate (`skill:use@<name>`, §19/§23) — enforced only
        // when the caller's capabilities are known. A whole-domain `skill:use`
        // covers any skill; a narrow `skill:use@<name>` covers only that one.
        if let Some(caps) = &ctx.capabilities {
            let required = Capability::new(Action::Use, Resource::new("skill", &skill.name));
            if !caps.iter().any(|c| c.covers(&required)) {
                return Err(Error::unauthorized(format!(
                    "the caller's grant does not permit skill:use@{}",
                    skill.name
                )));
            }
        }
        Ok(json!({
            "name": skill.name,
            "description": skill.description,
            "instructions": skill.instructions_md,
            "tools": skill.tools,
            "advertise_tools": skill.tools,
        }))
    }
}

/// `list_skills` — list the workspace's skills (name + description + tools), so
/// the model can discover what `use_skill` can invoke (SOUL §23).
pub(crate) struct ListSkillsTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for ListSkillsTool {
    fn name(&self) -> &str {
        "list_skills"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Read, "skill")
    }
    fn description(&self) -> &str {
        "List the available skills (name, description, and the tools each uses)."
    }
    fn parameters_schema(&self) -> Json {
        json!({ "type": "object", "properties": {} })
    }
    async fn invoke(&self, _args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let skills = self.store.skills().list_by_workspace(ws).await?;
        let items: Vec<Json> = skills
            .into_iter()
            .map(|s| {
                json!({
                    "name": s.name,
                    "description": s.description,
                    "tools": s.tools,
                    "advertised": s.advertised,
                })
            })
            .collect();
        Ok(json!({ "skills": items }))
    }
}

/// `create_skill` — author a new skill (SOUL §23): a named markdown runbook +
/// the restricted tool set it may use (+ optional executable code). The tool
/// twin of `POST /skills`, with the same `skill:write` gate and the same
/// normalization (trimmed name/description, cleaned tool list), so chat can grow
/// the workspace's skill library. Conflicts if the name exists — `edit_skill`
/// changes an existing skill.
pub(crate) struct CreateSkillTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for CreateSkillTool {
    fn name(&self) -> &str {
        "create_skill"
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "skill")
    }

    fn description(&self) -> &str {
        "Create a new named skill: a reusable markdown runbook plus the tools it \
         is meant to use. Fails if the name is already taken (use `edit_skill` to \
         change an existing skill). Returns a summary of the stored skill."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Unique (per workspace) skill name — how the skill is invoked." },
                "description": { "type": "string", "description": "One-line description shown when skills are listed. Optional." },
                "instructions_md": { "type": "string", "description": "Markdown runbook / instructions the skill's user follows. Optional." },
                "tools": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Tool names the skill may use (a subset of the registry). Optional."
                },
                "code": {
                    "type": "object",
                    "description": "Optional executable code attached to the skill, run via the executor.",
                    "properties": {
                        "language": { "type": "string", "description": "Language identifier, e.g. `python`." },
                        "source": { "type": "string", "description": "The source to execute." },
                        "entrypoint": { "type": "string", "description": "Optional pinned entrypoint." }
                    },
                    "required": ["language", "source"]
                },
                "advertised": {
                    "type": "boolean",
                    "description": "Whether the skill's name + description are advertised to the chat agent in its system prompt. Defaults to true."
                }
            },
            "required": ["name"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let spec = NewSkill {
            name: required_str(&args, "name")?,
            description: opt_str_some(&args, "description").unwrap_or_default(),
            instructions_md: opt_str(&args, "instructions_md"),
            tools: opt_skill_tools(&args),
            code: match args.get("code") {
                None | Some(Json::Null) => None,
                Some(v) => Some(skill_code(v)?),
            },
            advertised: args
                .get("advertised")
                .and_then(Json::as_bool)
                .unwrap_or(true),
        };
        let skill = self.store.skills().create(ws, &spec).await?;
        Ok(skill_summary(&skill))
    }
}

/// `edit_skill` — update an existing skill by name (SOUL §23). **Partial**:
/// only the supplied fields change (an omitted field keeps its stored value; an
/// explicit `"code": null` clears the code) — unlike the REST
/// `PUT /skills/{name}` full replacement, a chat edit usually touches one field.
/// Gated on the route's `skill:write` authority. Errors on an unknown name —
/// `create_skill` makes new ones. Read-merge-upsert, not a transaction: a
/// concurrent delete turns the upsert into a create, which still lands the
/// caller's intent (this definition under this name).
pub(crate) struct EditSkillTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for EditSkillTool {
    fn name(&self) -> &str {
        "edit_skill"
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "skill")
    }

    fn description(&self) -> &str {
        "Update an existing skill by name. Only the fields you pass change: an \
         omitted field keeps its current value, and `\"code\": null` clears the \
         attached code. The name itself cannot be changed. Returns a summary of \
         the updated skill."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Name of the skill to edit (not changeable)." },
                "description": { "type": "string", "description": "New one-line description. Omit to keep." },
                "instructions_md": { "type": "string", "description": "New markdown runbook (full replacement). Omit to keep." },
                "tools": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "New tool set (full replacement). Omit to keep."
                },
                "code": {
                    "type": "object",
                    "description": "New executable code (full replacement); pass null to clear. Omit to keep.",
                    "properties": {
                        "language": { "type": "string", "description": "Language identifier, e.g. `python`." },
                        "source": { "type": "string", "description": "The source to execute." },
                        "entrypoint": { "type": "string", "description": "Optional pinned entrypoint." }
                    },
                    "required": ["language", "source"]
                },
                "advertised": {
                    "type": "boolean",
                    "description": "New \"advertised in the chat system prompt\" flag. Omit to keep."
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
            .skills()
            .get_by_name(ws, &name)
            .await?
            .ok_or_else(|| {
                Error::invalid(format!(
                    "unknown skill `{name}` — use create_skill to make a new one"
                ))
            })?;
        let spec = NewSkill {
            name: existing.name,
            description: match args.get("description").and_then(Json::as_str) {
                Some(d) => d.trim().to_string(),
                None => existing.description,
            },
            instructions_md: match args.get("instructions_md").and_then(Json::as_str) {
                Some(md) => md.to_string(),
                None => existing.instructions_md,
            },
            tools: match args.get("tools") {
                Some(_) => opt_skill_tools(&args),
                None => existing.tools,
            },
            code: match args.get("code") {
                None => existing.code,
                Some(Json::Null) => None,
                Some(v) => Some(skill_code(v)?),
            },
            advertised: args
                .get("advertised")
                .and_then(Json::as_bool)
                .unwrap_or(existing.advertised),
        };
        let skill = self.store.skills().upsert_by_name(ws, &spec).await?;
        Ok(skill_summary(&skill))
    }
}
