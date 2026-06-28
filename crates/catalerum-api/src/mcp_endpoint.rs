//! Serving a user-authored, Boa-scripted MCP endpoint (SOUL §26).
//!
//! An [`McpEndpoint`](catalerum_core::model::McpEndpoint) is a stored JavaScript
//! program that **declares** MCP tools and **implements** their `tools/call`. It
//! runs against a deliberately tiny host bridge — the script may only reach
//! `catalerum.callTool("search_semantic", …)`, and the endpoint's configured
//! `bucket_name` + `key_prefix` scope is **injected** into every such call, so a
//! script can never widen its own reach (the "one wiki subdir" guarantee, enforced
//! at the bridge *and* the Qdrant filter — never via capability constraints, which
//! have a known attenuation gap).
//!
//! Each endpoint gets its **own** [`McpServer`] over a purpose-built
//! [`ToolRegistry`] holding only its script-declared tools — it is *not* folded
//! into the main `/mcp` registry, so a custom endpoint never leaks tools into any
//! other surface. `tools/list` shows only the script's tools; `search_semantic`
//! is reachable solely through the host bridge, never advertised.
//!
//! ## Script contract
//! The script is a function body (the `catalerum-script` UI convention) that reads
//! its bound `input` and `return`s a value. `input.method` selects the phase:
//! - `"tools/list"` → return `[{ name, description?, inputSchema? }, …]`.
//! - `"tools/call"` → `input.name` + `input.arguments`; run that tool and return
//!   its result (any JSON), typically via
//!   `catalerum.callTool("search_semantic", { query })`.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use catalerum_core::capability::{Action, Capability, Resource};
use catalerum_core::model::McpEndpoint;
use catalerum_core::tool::{Tool, ToolContext, ToolRegistry};
use catalerum_core::{Error, Result, WorkspaceId};
use catalerum_mcp::McpServer;
use catalerum_script::{ScriptCodeRunner, UiScriptHost};

/// The one host tool an endpoint script may reach.
const ALLOWED_TOOL: &str = "search_semantic";

/// The minimal authority an endpoint runs under when it has no explicit grant:
/// just semantic search, nothing else. Its subdir scope is pinned by the host, so
/// even this can only ever read the endpoint's slice.
#[must_use]
pub fn default_endpoint_caps() -> Vec<Capability> {
    vec![Capability::new(Action::Search, Resource::domain("vector"))]
}

/// The [`UiScriptHost`] an endpoint script reaches: `call_tool` allow-lists
/// `search_semantic`, **overrides** `bucket_name`/`key_prefix` with the endpoint's
/// pinned scope (so a script cannot widen it), then `block_on`s the async
/// dispatch under the endpoint's authority (`ctx`). The `block_on` is valid — it
/// runs on the script's `spawn_blocking` thread, never a runtime worker.
struct EndpointToolHost {
    registry: ToolRegistry,
    handle: tokio::runtime::Handle,
    ctx: ToolContext,
    bucket_name: Option<String>,
    key_prefix: Option<String>,
}

impl UiScriptHost for EndpointToolHost {
    fn call_tool(&self, tool: &str, args: Value) -> std::result::Result<Value, String> {
        if tool != ALLOWED_TOOL {
            return Err(format!(
                "an MCP endpoint script may only call `{ALLOWED_TOOL}` (got `{tool}`)"
            ));
        }
        // Pin the scope: start from whatever the script passed, then force our
        // bucket/prefix on top so the script's own values can't widen the reach.
        let mut map = args.as_object().cloned().unwrap_or_default();
        if let Some(bucket) = &self.bucket_name {
            map.insert("bucket_name".into(), json!(bucket));
        }
        if let Some(prefix) = &self.key_prefix {
            map.insert("key_prefix".into(), json!(prefix));
        }
        self.handle
            .block_on(
                self.registry
                    .dispatch(ALLOWED_TOOL, Value::Object(map), &self.ctx),
            )
            .map_err(|e| e.to_string())
    }
}

/// One MCP tool declared by an endpoint script. `invoke` re-enters the script with
/// `{ method: "tools/call", name, arguments }` and returns its result. It requires
/// **no capability of its own** — the real authority boundary is the host bridge's
/// `ctx` when the script calls `search_semantic`, so the outer `McpServer` dispatch
/// need not (and does not) re-gate it.
struct ScriptBackedTool {
    tool_name: String,
    description: String,
    schema: Value,
    script: Arc<String>,
    host: Arc<EndpointToolHost>,
}

#[async_trait]
impl Tool for ScriptBackedTool {
    fn name(&self) -> &str {
        &self.tool_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> Value {
        self.schema.clone()
    }

    async fn invoke(&self, args: Value, _ctx: &ToolContext) -> Result<Value> {
        let input = json!({
            "method": "tools/call",
            "name": self.tool_name,
            "arguments": args,
        });
        let outcome = ScriptCodeRunner::new()
            .run_ui_script(&self.script, &input, &json!({}), self.host.clone())
            .await
            .map_err(|e| Error::provider(format!("endpoint tool `{}`: {e}", self.tool_name)))?;
        Ok(outcome.returned)
    }
}

/// Build the [`McpServer`] that serves one endpoint: run the script's `tools/list`
/// to discover its declared tools, wrap each as a [`ScriptBackedTool`] in a fresh
/// isolated registry, and return the server. `caps` is the endpoint's authority
/// (the grant's capabilities, or [`default_endpoint_caps`]); it must include
/// `search:vector` for the script's search calls to pass dispatch.
pub async fn build_endpoint_server(
    endpoint: &McpEndpoint,
    registry: ToolRegistry,
    workspace_id: WorkspaceId,
    caps: Vec<Capability>,
) -> std::result::Result<McpServer, String> {
    // The authority the host bridge dispatches `search_semantic` under.
    let host_ctx = ToolContext {
        workspace_id: Some(workspace_id),
        capabilities: Some(caps),
        ..Default::default()
    };
    let host = Arc::new(EndpointToolHost {
        registry,
        handle: tokio::runtime::Handle::current(),
        ctx: host_ctx,
        bucket_name: endpoint.bucket_name.clone(),
        key_prefix: endpoint.key_prefix.clone(),
    });
    let script = Arc::new(endpoint.script.clone());

    // Ask the script which tools it exposes.
    let listed = ScriptCodeRunner::new()
        .run_ui_script(
            &script,
            &json!({ "method": "tools/list" }),
            &json!({}),
            host.clone(),
        )
        .await
        .map_err(|e| format!("endpoint `{}` tools/list: {e}", endpoint.name))?;

    let specs = listed.returned.as_array().cloned().unwrap_or_default();
    let mut tools = ToolRegistry::new();
    for spec in &specs {
        let Some(name) = spec.get("name").and_then(Value::as_str) else {
            continue;
        };
        let description = spec
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let schema = spec
            .get("inputSchema")
            .cloned()
            .unwrap_or_else(|| json!({ "type": "object" }));
        tools.register(Arc::new(ScriptBackedTool {
            tool_name: name.to_string(),
            description,
            schema,
            script: script.clone(),
            host: host.clone(),
        }));
    }

    // The outer server dispatches ScriptBackedTools (which require no capability),
    // so its own ctx carries no cap set — the endpoint authority lives in the host
    // bridge's ctx above.
    let server_ctx = ToolContext {
        workspace_id: Some(workspace_id),
        ..Default::default()
    };
    Ok(McpServer::new(tools, server_ctx).with_server_info(
        format!("catalerum:{}", endpoint.name),
        env!("CARGO_PKG_VERSION"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for `search_semantic` that echoes the args it received — enough
    /// to prove what the host bridge dispatched.
    struct EchoSearch;

    #[async_trait]
    impl Tool for EchoSearch {
        fn name(&self) -> &str {
            "search_semantic"
        }
        fn parameters_schema(&self) -> Value {
            json!({ "type": "object" })
        }
        async fn invoke(&self, args: Value, _ctx: &ToolContext) -> Result<Value> {
            Ok(args)
        }
    }

    fn host(bucket: Option<&str>, prefix: Option<&str>) -> EndpointToolHost {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(EchoSearch));
        EndpointToolHost {
            registry: reg,
            handle: tokio::runtime::Handle::current(),
            ctx: ToolContext::default(),
            bucket_name: bucket.map(str::to_string),
            key_prefix: prefix.map(str::to_string),
        }
    }

    /// The host **overrides** the scope a script supplies — a script that asks for
    /// `key_prefix: "other/"` still only ever searches the endpoint's pinned
    /// `acme/`, so it can never widen its reach.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn host_pins_scope_and_blocks_widening() {
        let h = host(Some("wiki"), Some("acme/"));
        let out = tokio::task::spawn_blocking(move || {
            h.call_tool(
                "search_semantic",
                json!({ "query": "x", "key_prefix": "other/" }),
            )
        })
        .await
        .unwrap()
        .expect("call ok");
        assert_eq!(
            out["key_prefix"], "acme/",
            "prefix must be pinned, not widened"
        );
        assert_eq!(out["bucket_name"], "wiki");
        assert_eq!(out["query"], "x");
    }

    /// A script may reach *only* `search_semantic` — any other tool is refused at
    /// the bridge, before dispatch.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn host_rejects_non_search_tool() {
        let h = host(None, None);
        let err = tokio::task::spawn_blocking(move || h.call_tool("create_note", json!({})))
            .await
            .unwrap()
            .expect_err("must reject");
        assert!(err.contains("search_semantic"), "unexpected: {err}");
    }
}
