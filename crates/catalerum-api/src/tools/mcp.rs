//! MCP server management tools.

use super::*;

/// The JSON-Schema for a server definition (shared by create/edit). `name` is the
/// only hard requirement; `transport` selects stdio (default) vs http.
pub(crate) fn mcp_server_schema(name_required: bool) -> Json {
    let required: Json = if name_required {
        json!(["name"])
    } else {
        json!([])
    };
    json!({
        "type": "object",
        "properties": {
            "name": { "type": "string", "description": "Unique server name; prefixes its tools and scopes `mcp:use@{name}`." },
            "transport": { "type": "string", "enum": ["stdio", "http"], "description": "`stdio` (spawn `command`) or `http` (connect to `url`). Default stdio." },
            "command": { "type": "string", "description": "Program to spawn (stdio), e.g. `npx`." },
            "args": { "type": "array", "items": { "type": "string" }, "description": "Arguments to `command` (stdio)." },
            "env": { "type": "object", "additionalProperties": { "type": "string" }, "description": "Extra environment for the child (stdio)." },
            "url": { "type": "string", "description": "Endpoint URL (http), e.g. https://host/mcp." },
            "auth": {
                "type": "object",
                "description": "HTTP auth. `kind`: none|bearer|header|oauth2.",
                "properties": {
                    "kind": { "type": "string", "enum": ["none", "bearer", "header", "oauth2"] },
                    "token": { "type": "string", "description": "bearer token" },
                    "header_name": { "type": "string" },
                    "header_value": { "type": "string" },
                    "token_url": { "type": "string", "description": "oauth2 token endpoint" },
                    "grant_type": { "type": "string", "enum": ["client_credentials", "refresh_token"] },
                    "client_id": { "type": "string" },
                    "client_secret": { "type": "string" },
                    "refresh_token": { "type": "string" },
                    "scope": { "type": "string" }
                }
            },
            "enabled": { "type": "boolean", "description": "Connect this server (default true)." },
            "tools": { "type": "array", "items": { "type": "string" }, "description": "Allow-list of remote tool names to import; omit = all." }
        },
        "required": required
    })
}

/// Parse the shared server-definition args into a [`NewMcpServerDef`], validating
/// that the transport has what it needs (a `command` for stdio, a `url` for http).
pub(crate) fn parse_mcp_server(args: &Json) -> Result<NewMcpServerDef> {
    let name = required_str(args, "name")?;
    let transport = {
        let t = opt_str(args, "transport");
        if t.trim().is_empty() {
            "stdio".to_string()
        } else {
            t.trim().to_ascii_lowercase()
        }
    };
    let is_http = matches!(
        transport.as_str(),
        "http" | "https" | "sse" | "streamable-http"
    );
    let command = opt_str(args, "command");
    let url = opt_str(args, "url");
    if is_http && url.trim().is_empty() {
        return Err(Error::invalid("`url` is required for an http MCP server"));
    }
    if !is_http && command.trim().is_empty() {
        return Err(Error::invalid(
            "`command` is required for a stdio MCP server",
        ));
    }
    let env: BTreeMap<String, String> = args
        .get("env")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| Error::invalid(format!("`env` must be an object of strings: {e}")))?
        .unwrap_or_default();
    let auth: McpAuthSpec = args
        .get("auth")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| Error::invalid(format!("`auth` is malformed: {e}")))?
        .unwrap_or_default();
    Ok(NewMcpServerDef {
        name,
        transport,
        command,
        args: opt_str_vec(args, "args"),
        env,
        url,
        auth,
        enabled: args.get("enabled").and_then(Json::as_bool).unwrap_or(true),
        tools: opt_str_vec(args, "tools"),
    })
}

/// A redacted, agent-safe view of a stored server: secrets are never echoed back
/// (only whether they are configured), and `env` shows keys, not values.
pub(crate) fn redact_mcp_server(def: &McpServerDef, connected: bool) -> Json {
    let env_keys: Vec<&String> = def.env.keys().collect();
    let auth_kind = if def.auth.kind.trim().is_empty() {
        "none"
    } else {
        def.auth.kind.as_str()
    };
    json!({
        "name": def.name,
        "transport": def.transport,
        "command": def.command,
        "args": def.args,
        "env_keys": env_keys,
        "url": def.url,
        "auth_kind": auth_kind,
        "auth_has_secret": def.auth.has_secret(),
        "enabled": def.enabled,
        "tools": def.tools,
        "connected": connected,
        "created_at": def.created_at.to_rfc3339(),
        "updated_at": def.updated_at.to_rfc3339(),
    })
}

/// `list_mcp_servers` — list the workspace's external MCP servers (SOUL §26),
/// secrets redacted, with live connection status. Gated on `mcp:read`.
pub(crate) struct ListMcpServersTool {
    pub(crate) store: Store,
    pub(crate) manager: McpManager,
}

#[async_trait]
impl Tool for ListMcpServersTool {
    fn name(&self) -> &str {
        "list_mcp_servers"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Read, "mcp")
    }
    fn description(&self) -> &str {
        "List the workspace's external MCP servers (name, transport, target, \
         enabled, live connection status). Secrets are redacted."
    }
    fn parameters_schema(&self) -> Json {
        json!({ "type": "object", "properties": {} })
    }
    async fn invoke(&self, _args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let servers = self.store.mcp_servers().list_by_workspace(ws).await?;
        let items: Vec<Json> = servers
            .iter()
            .map(|s| redact_mcp_server(s, self.manager.is_connected(&s.name)))
            .collect();
        Ok(json!({ "servers": items }))
    }
}

/// `create_mcp_server` — persist a new external MCP server and connect it live
/// (SOUL §26). Fails if the name already exists (use `edit_mcp_server` to change
/// one). Gated on `mcp:write`.
pub(crate) struct CreateMcpServerTool {
    pub(crate) store: Store,
    pub(crate) manager: McpManager,
}

#[async_trait]
impl Tool for CreateMcpServerTool {
    fn name(&self) -> &str {
        "create_mcp_server"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "mcp")
    }
    fn description(&self) -> &str {
        "Add an external MCP server (stdio: spawn a command; http: connect to a \
         URL with optional bearer/header/oauth2 auth). Persists it and connects \
         it live — its tools (named `{name}_{tool}`) become callable immediately, \
         each requiring the `mcp:use@{name}` capability. Fails if the name exists."
    }
    fn parameters_schema(&self) -> Json {
        mcp_server_schema(true)
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let new = parse_mcp_server(&args)?;
        let def = self.store.mcp_servers().create(ws, &new).await?;
        // Persisted. Now connect live (best-effort): report the outcome, but a
        // connect failure doesn't un-persist — it'll retry on the next boot.
        Ok(connect_outcome(&self.manager, &def, "created").await)
    }
}

/// `edit_mcp_server` — create-or-replace a server by name and reconnect it (SOUL
/// §26). Gated on `mcp:write`.
pub(crate) struct EditMcpServerTool {
    pub(crate) store: Store,
    pub(crate) manager: McpManager,
}

#[async_trait]
impl Tool for EditMcpServerTool {
    fn name(&self) -> &str {
        "edit_mcp_server"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "mcp")
    }
    fn description(&self) -> &str {
        "Create or replace an external MCP server by name, then reconnect it: its \
         old tools are dropped and the new definition's tools imported (or, if \
         `enabled` is false, it is disconnected). Same fields as create_mcp_server."
    }
    fn parameters_schema(&self) -> Json {
        mcp_server_schema(true)
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let new = parse_mcp_server(&args)?;
        let def = self.store.mcp_servers().upsert_by_name(ws, &new).await?;
        if def.enabled {
            Ok(connect_outcome(&self.manager, &def, "updated").await)
        } else {
            // Disabled on edit → ensure any live tools are torn down.
            let removed = self.manager.disconnect(&def.name);
            Ok(
                json!({ "updated": true, "name": def.name, "connected": false, "removed_tools": removed }),
            )
        }
    }
}

/// `delete_mcp_server` — disconnect and remove a server by name (SOUL §26). Gated
/// on `mcp:delete`.
pub(crate) struct DeleteMcpServerTool {
    pub(crate) store: Store,
    pub(crate) manager: McpManager,
}

#[async_trait]
impl Tool for DeleteMcpServerTool {
    fn name(&self) -> &str {
        "delete_mcp_server"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Delete, "mcp")
    }
    fn description(&self) -> &str {
        "Remove an external MCP server by name: disconnect its live tools, then \
         delete its stored definition. Errors if no such server exists."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": { "name": { "type": "string", "description": "The server name to delete." } },
            "required": ["name"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let name = required_str(&args, "name")?;
        // Tear down live tools first, then delete the row (NotFound if absent).
        let removed = self.manager.disconnect(&name);
        self.store.mcp_servers().delete_by_name(ws, &name).await?;
        Ok(json!({ "deleted": true, "name": name, "removed_tools": removed }))
    }
}

/// Connect (or reconnect) `def` through the manager and render the tool result —
/// a connect error is reported, not raised, because the definition is already
/// persisted and will retry on the next boot. `verb` is the past-tense action
/// (`created`/`updated`) keyed `true` in the result.
pub(crate) async fn connect_outcome(manager: &McpManager, def: &McpServerDef, verb: &str) -> Json {
    let mut out = serde_json::Map::new();
    out.insert(verb.to_string(), json!(true));
    out.insert("name".to_string(), json!(def.name));
    if !def.enabled {
        out.insert("connected".to_string(), json!(false));
        out.insert(
            "note".to_string(),
            json!("server is disabled; not connected"),
        );
        return Json::Object(out);
    }
    match manager.connect(def).await {
        Ok(n) => {
            out.insert("connected".to_string(), json!(true));
            out.insert("imported_tools".to_string(), json!(n));
        }
        Err(e) => {
            out.insert("connected".to_string(), json!(false));
            out.insert("connect_error".to_string(), json!(e.to_string()));
        }
    }
    Json::Object(out)
}

/// Register the external-MCP-server management tools (SOUL §26) into `registry`.
/// Built post-`build_registry` because they need the live [`McpManager`]; each is
/// admin-gated on the `mcp` domain (§19), so a base role can't reach them.
pub(crate) fn register_mcp_tools(registry: &mut ToolRegistry, store: &Store, manager: &McpManager) {
    registry.register(Arc::new(ListMcpServersTool {
        store: store.clone(),
        manager: manager.clone(),
    }));
    registry.register(Arc::new(CreateMcpServerTool {
        store: store.clone(),
        manager: manager.clone(),
    }));
    registry.register(Arc::new(EditMcpServerTool {
        store: store.clone(),
        manager: manager.clone(),
    }));
    registry.register(Arc::new(DeleteMcpServerTool {
        store: store.clone(),
        manager: manager.clone(),
    }));
}

// ===========================================================================
// Automations authoring (SOUL §11) — let an agent create / edit / test / run the
// workspace's automations, the same surface the REST routes + visual editor use.
// ===========================================================================
