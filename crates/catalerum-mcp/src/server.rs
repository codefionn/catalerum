//! The MCP server: exposes catalerum's [`ToolRegistry`] (SOUL §7) as MCP tools so
//! external agents — Claude Code, Codex, opencode — are first-class clients
//! (principle 15). It is **the same scoped tool surface in MCP clothing, no
//! backdoor**: every `tools/call` dispatches through the shared registry under a
//! fixed [`ToolContext`] (the MCP client's workspace + capability grant, §19/§26),
//! so an external agent gets exactly the slice it was granted, deny-by-default.
//!
//! Implemented JSON-RPC methods: `initialize` (handshake); `tools/list` /
//! `tools/call` (§26 Tools); `prompts/list` / `prompts/get` (§26 Prompts — the
//! workspace's skills §23, when a [`PromptProvider`] is attached); `resources/list`
//! / `resources/read` (§26 Resources — read views, when a [`ResourceProvider`] is
//! attached); and `ping`. The token→grant resolution is a later slice.

use std::sync::Arc;

use serde_json::{json, Value};

use catalerum_core::tool::{ToolContext, ToolRegistry};
use catalerum_core::Error as CoreError;

use crate::prompts::{PromptContent, PromptProvider};
use crate::protocol::{JsonRpcRequest, JsonRpcResponse, INVALID_PARAMS, METHOD_NOT_FOUND};
use crate::resources::{ResourceContent, ResourceProvider};

/// The MCP protocol version this server advertises.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// An MCP server over a [`ToolRegistry`], scoped by a fixed [`ToolContext`].
///
/// The context carries the MCP client's `workspace_id` + capability grant (§19):
/// `tools/call` dispatches under it, so capability enforcement is identical to a
/// web/agent call — the registry is the single choke point.
#[derive(Clone)]
pub struct McpServer {
    registry: ToolRegistry,
    ctx: ToolContext,
    prompts: Option<Arc<dyn PromptProvider>>,
    resources: Option<Arc<dyn ResourceProvider>>,
    name: String,
    version: String,
}

impl McpServer {
    /// Build a server exposing `registry` under the authority of `ctx`.
    #[must_use]
    pub fn new(registry: ToolRegistry, ctx: ToolContext) -> Self {
        Self {
            registry,
            ctx,
            prompts: None,
            resources: None,
            name: "catalerum".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    /// Attach a [`PromptProvider`] (the workspace's skills §23) so the server also
    /// serves `prompts/list` / `prompts/get` and advertises the `prompts` capability.
    #[must_use]
    pub fn with_prompts(mut self, prompts: Arc<dyn PromptProvider>) -> Self {
        self.prompts = Some(prompts);
        self
    }

    /// Attach a [`ResourceProvider`] (read views §26) so the server also serves
    /// `resources/list` / `resources/read` and advertises the `resources` capability.
    #[must_use]
    pub fn with_resources(mut self, resources: Arc<dyn ResourceProvider>) -> Self {
        self.resources = Some(resources);
        self
    }

    /// Override the advertised server name/version (the `serverInfo`).
    #[must_use]
    pub fn with_server_info(mut self, name: impl Into<String>, version: impl Into<String>) -> Self {
        self.name = name.into();
        self.version = version.into();
        self
    }

    /// Handle one JSON-RPC request, returning the response — or `None` for a
    /// notification (which must produce no reply, per JSON-RPC).
    pub async fn handle(&self, req: JsonRpcRequest) -> Option<JsonRpcResponse> {
        // A notification (no id) never gets a response; we just ignore unknown ones.
        if req.is_notification() {
            return None;
        }
        let id = req.id.clone().unwrap_or(Value::Null);
        let resp = match req.method.as_str() {
            "initialize" => JsonRpcResponse::ok(id, self.initialize_result()),
            "tools/list" => JsonRpcResponse::ok(id, self.tools_list()),
            "tools/call" => self.tools_call(id, req.params).await,
            "prompts/list" => JsonRpcResponse::ok(id, self.prompts_list().await),
            "prompts/get" => self.prompts_get(id, req.params).await,
            "resources/list" => JsonRpcResponse::ok(id, self.resources_list().await),
            "resources/read" => self.resources_read(id, req.params).await,
            "ping" => JsonRpcResponse::ok(id, json!({})),
            other => {
                JsonRpcResponse::error(id, METHOD_NOT_FOUND, format!("method not found: {other}"))
            }
        };
        Some(resp)
    }

    fn initialize_result(&self) -> Value {
        // Advertise `prompts` only when a provider is attached (no list-changed
        // notifications yet for either).
        let mut capabilities = json!({ "tools": {} });
        if self.prompts.is_some() {
            capabilities["prompts"] = json!({});
        }
        if self.resources.is_some() {
            capabilities["resources"] = json!({});
        }
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": capabilities,
            "serverInfo": { "name": self.name, "version": self.version },
        })
    }

    /// `tools/list` → the registry's tools as MCP tool descriptors (`inputSchema`
    /// is the tool's JSON-Schema parameters), sorted by name for a stable listing.
    fn tools_list(&self) -> Value {
        let mut tools: Vec<Value> = self
            .registry
            .specs(None)
            .into_iter()
            .map(|s| json!({ "name": s.name, "description": s.description, "inputSchema": s.parameters }))
            .collect();
        tools.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        json!({ "tools": tools })
    }

    /// `tools/call` → dispatch `{ name, arguments }` through the registry under the
    /// server's scope. The tool result is wrapped as an MCP text content block;
    /// a **tool/authorization failure** is reported as `isError: true` content (so
    /// the calling agent sees it and can recover), while a structural problem — a
    /// missing `name` or an unknown tool — is a JSON-RPC `INVALID_PARAMS` error.
    async fn tools_call(&self, id: Value, params: Option<Value>) -> JsonRpcResponse {
        let params = params.unwrap_or(Value::Null);
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return JsonRpcResponse::error(id, INVALID_PARAMS, "tools/call requires a `name`");
        };
        if !self.registry.contains(name) {
            return JsonRpcResponse::error(id, INVALID_PARAMS, format!("unknown tool: {name}"));
        }
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        match self.registry.dispatch(name, args, &self.ctx).await {
            Ok(result) => JsonRpcResponse::ok(id, tool_content(&result, false)),
            // Unauthorized + the tool's own errors surface as isError content, the
            // MCP convention for a call that reached the tool but failed.
            Err(e) => JsonRpcResponse::ok(id, tool_content(&error_text(&e), true)),
        }
    }

    /// `prompts/list` → the provider's prompts (the workspace's skills §23), sorted
    /// by name. Empty when no provider is attached.
    async fn prompts_list(&self) -> Value {
        let mut prompts: Vec<Value> = match &self.prompts {
            Some(p) => p
                .list()
                .await
                .into_iter()
                .map(|i| json!({ "name": i.name, "description": i.description }))
                .collect(),
            None => Vec::new(),
        };
        prompts.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        json!({ "prompts": prompts })
    }

    /// `prompts/get` → one prompt's content as a single user message. No provider →
    /// `METHOD_NOT_FOUND`; missing `name` or an unknown prompt → `INVALID_PARAMS`.
    async fn prompts_get(&self, id: Value, params: Option<Value>) -> JsonRpcResponse {
        let Some(provider) = &self.prompts else {
            return JsonRpcResponse::error(id, METHOD_NOT_FOUND, "this server exposes no prompts");
        };
        let params = params.unwrap_or(Value::Null);
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return JsonRpcResponse::error(id, INVALID_PARAMS, "prompts/get requires a `name`");
        };
        match provider.get(name).await {
            Some(content) => JsonRpcResponse::ok(id, prompt_content(&content)),
            None => JsonRpcResponse::error(id, INVALID_PARAMS, format!("unknown prompt: {name}")),
        }
    }

    /// `resources/list` → the provider's read views, sorted by uri. Empty when no
    /// provider is attached.
    async fn resources_list(&self) -> Value {
        let mut resources: Vec<Value> = match &self.resources {
            Some(p) => p
                .list()
                .await
                .into_iter()
                .map(|r| {
                    json!({ "uri": r.uri, "name": r.name, "description": r.description, "mimeType": r.mime_type })
                })
                .collect(),
            None => Vec::new(),
        };
        resources.sort_by(|a, b| a["uri"].as_str().cmp(&b["uri"].as_str()));
        json!({ "resources": resources })
    }

    /// `resources/read` → one resource's content. No provider → `METHOD_NOT_FOUND`;
    /// missing `uri` or an unknown resource → `INVALID_PARAMS`.
    async fn resources_read(&self, id: Value, params: Option<Value>) -> JsonRpcResponse {
        let Some(provider) = &self.resources else {
            return JsonRpcResponse::error(
                id,
                METHOD_NOT_FOUND,
                "this server exposes no resources",
            );
        };
        let params = params.unwrap_or(Value::Null);
        let Some(uri) = params.get("uri").and_then(Value::as_str) else {
            return JsonRpcResponse::error(id, INVALID_PARAMS, "resources/read requires a `uri`");
        };
        match provider.read(uri).await {
            Some(content) => JsonRpcResponse::ok(id, resource_content(&content)),
            None => JsonRpcResponse::error(id, INVALID_PARAMS, format!("unknown resource: {uri}")),
        }
    }
}

/// Render a [`ResourceContent`] as an MCP `resources/read` result: a single text
/// content part under `contents`.
fn resource_content(content: &ResourceContent) -> Value {
    json!({
        "contents": [{
            "uri": content.uri,
            "mimeType": content.mime_type,
            "text": content.text,
        }],
    })
}

/// Render a [`PromptContent`] as an MCP `prompts/get` result: a single user
/// message carrying the body, plus the optional description.
fn prompt_content(content: &PromptContent) -> Value {
    let mut result = json!({
        "messages": [{
            "role": "user",
            "content": { "type": "text", "text": content.text },
        }],
    });
    if let Some(description) = &content.description {
        result["description"] = json!(description);
    }
    result
}

/// Wrap a tool outcome as an MCP `tools/call` result: a single text content block
/// plus the `isError` flag. The text is the JSON result serialized (or, for an
/// error, the message).
fn tool_content(payload: &Value, is_error: bool) -> Value {
    let text = match payload {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

/// A human-readable message for a dispatch error (for the isError content).
fn error_text(e: &CoreError) -> Value {
    Value::String(e.to_string())
}
