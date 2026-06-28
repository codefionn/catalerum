//! MCP server protocol tests (SOUL §26): the JSON-RPC handshake, tool listing,
//! tool dispatch through the shared registry, capability scoping (deny-by-default),
//! error mapping, and a stdio transport round-trip. Pure (no DB/network) — a tiny
//! in-test registry stands in for the real §7 tools.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::BufReader;

use catalerum_core::capability::{Action, Capability, Resource};
use catalerum_core::tool::{Tool, ToolContext, ToolRegistry};
use catalerum_core::Result as CoreResult;
use catalerum_mcp::{
    serve, JsonRpcRequest, McpServer, PromptContent, PromptInfo, PromptProvider, ResourceContent,
    ResourceInfo, ResourceProvider,
};

/// A static resource source (stands in for the notes/tasks-backed provider).
struct StaticResources;
#[async_trait]
impl ResourceProvider for StaticResources {
    async fn list(&self) -> Vec<ResourceInfo> {
        vec![ResourceInfo {
            uri: "catalerum://notes".into(),
            name: "Notes".into(),
            description: "Workspace notes".into(),
            mime_type: "text/markdown".into(),
        }]
    }
    async fn read(&self, uri: &str) -> Option<ResourceContent> {
        (uri == "catalerum://notes").then(|| ResourceContent {
            uri: uri.into(),
            mime_type: "text/markdown".into(),
            text: "# Notes\n\n- hello".into(),
        })
    }
}

/// A server with a resource provider attached.
fn server_with_resources() -> McpServer {
    McpServer::new(registry(), ToolContext::default()).with_resources(Arc::new(StaticResources))
}

/// A static prompt source (stands in for the skill-backed provider).
struct StaticPrompts;
#[async_trait]
impl PromptProvider for StaticPrompts {
    async fn list(&self) -> Vec<PromptInfo> {
        vec![PromptInfo {
            name: "summarize".into(),
            description: "Summarize text".into(),
        }]
    }
    async fn get(&self, name: &str) -> Option<PromptContent> {
        (name == "summarize").then(|| PromptContent {
            description: Some("Summarize text".into()),
            text: "Summarize the input concisely.".into(),
        })
    }
}

/// A server with a prompt provider attached.
fn server_with_prompts() -> McpServer {
    McpServer::new(registry(), ToolContext::default()).with_prompts(Arc::new(StaticPrompts))
}

/// An ungated tool that echoes its arguments.
struct EchoTool;
#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "Echo the arguments back"
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": { "msg": { "type": "string" } } })
    }
    async fn invoke(&self, args: Value, _ctx: &ToolContext) -> CoreResult<Value> {
        Ok(json!({ "echoed": args }))
    }
}

/// A tool gated on `notes:write` — to exercise §19/§26 capability scoping.
struct WriteNoteTool;
#[async_trait]
impl Tool for WriteNoteTool {
    fn name(&self) -> &str {
        "write_note"
    }
    fn required_capability(&self) -> Option<Capability> {
        Some(Capability::new(Action::Write, Resource::domain("notes")))
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object" })
    }
    async fn invoke(&self, _args: Value, _ctx: &ToolContext) -> CoreResult<Value> {
        Ok(json!({ "created": true }))
    }
}

fn registry() -> ToolRegistry {
    let mut r = ToolRegistry::new();
    r.register(Arc::new(EchoTool));
    r.register(Arc::new(WriteNoteTool));
    r
}

/// A server scoped to `capabilities` (`None` → enforcement off; `Some` → deny-by-default).
fn server(capabilities: Option<Vec<Capability>>) -> McpServer {
    let ctx = ToolContext {
        capabilities,
        ..Default::default()
    };
    McpServer::new(registry(), ctx)
}

fn req(id: i64, method: &str, params: Value) -> JsonRpcRequest {
    serde_json::from_value(
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }),
    )
    .unwrap()
}

#[tokio::test]
async fn initialize_returns_protocol_and_server_info() {
    let resp = server(None)
        .handle(req(1, "initialize", json!({})))
        .await
        .unwrap();
    let result = resp.result.expect("result");
    assert_eq!(result["protocolVersion"], json!("2025-06-18"));
    assert!(result["capabilities"]["tools"].is_object());
    assert_eq!(result["serverInfo"]["name"], json!("catalerum"));
    assert!(result["serverInfo"]["version"].is_string());
}

#[tokio::test]
async fn tools_list_advertises_the_registry_sorted() {
    let resp = server(None)
        .handle(req(2, "tools/list", json!({})))
        .await
        .unwrap();
    let tools = resp.result.unwrap()["tools"].as_array().unwrap().clone();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        vec!["echo", "write_note"],
        "all tools, sorted by name"
    );
    let echo = &tools[0];
    assert_eq!(echo["description"], json!("Echo the arguments back"));
    // The JSON-Schema parameters are exposed as MCP `inputSchema`.
    assert_eq!(echo["inputSchema"]["type"], json!("object"));
}

#[tokio::test]
async fn tools_call_dispatches_and_wraps_the_result() {
    let resp = server(None)
        .handle(req(
            3,
            "tools/call",
            json!({ "name": "echo", "arguments": { "msg": "hi" } }),
        ))
        .await
        .unwrap();
    let result = resp.result.expect("result");
    assert_eq!(result["isError"], json!(false));
    let text = result["content"][0]["text"].as_str().unwrap();
    // The text is the tool's JSON result serialized.
    let parsed: Value = serde_json::from_str(text).unwrap();
    assert_eq!(parsed["echoed"]["msg"], json!("hi"));
}

#[tokio::test]
async fn unknown_tool_and_missing_name_are_invalid_params() {
    let s = server(None);
    let unknown = s
        .handle(req(4, "tools/call", json!({ "name": "nope" })))
        .await
        .unwrap();
    assert_eq!(unknown.error.unwrap().code, -32602);
    let missing = s.handle(req(5, "tools/call", json!({}))).await.unwrap();
    assert_eq!(missing.error.unwrap().code, -32602);
}

#[tokio::test]
async fn unknown_method_is_method_not_found() {
    let resp = server(None)
        .handle(req(6, "logging/setLevel", json!({})))
        .await
        .unwrap();
    assert_eq!(resp.error.unwrap().code, -32601);
}

#[tokio::test]
async fn capability_scoping_is_deny_by_default() {
    // No capabilities granted → the gated tool is denied (isError content, not a
    // crash) — the same deny-by-default gate as a web/agent call (§19/§26).
    let denied = server(Some(vec![]))
        .handle(req(
            7,
            "tools/call",
            json!({ "name": "write_note", "arguments": {} }),
        ))
        .await
        .unwrap();
    let r = denied
        .result
        .expect("isError content, not a protocol error");
    assert_eq!(r["isError"], json!(true));
    assert!(r["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_lowercase()
        .contains("notes"));

    // Granting notes:write lets it through.
    let allowed = server(Some(vec![Capability::new(
        Action::Write,
        Resource::domain("notes"),
    )]))
    .handle(req(
        8,
        "tools/call",
        json!({ "name": "write_note", "arguments": {} }),
    ))
    .await
    .unwrap();
    let r = allowed.result.unwrap();
    assert_eq!(r["isError"], json!(false));
    assert!(r["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("created"));
}

#[tokio::test]
async fn a_notification_gets_no_response() {
    // No `id` → a notification → the server must not reply.
    let notif: JsonRpcRequest =
        serde_json::from_value(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .unwrap();
    assert!(server(None).handle(notif).await.is_none());
}

#[tokio::test]
async fn stdio_transport_round_trips_requests() {
    // Two requests + a notification (no reply) over the line-delimited transport.
    let input = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"echo","arguments":{"x":1}}}"#,
        "\n",
    );
    let mut output: Vec<u8> = Vec::new();
    serve(&server(None), BufReader::new(input.as_bytes()), &mut output)
        .await
        .expect("serve");

    let lines: Vec<Value> = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    // The notification produced no line → exactly two responses.
    assert_eq!(
        lines.len(),
        2,
        "two requests answered, the notification ignored"
    );
    assert_eq!(lines[0]["id"], json!(1));
    assert_eq!(lines[0]["result"]["protocolVersion"], json!("2025-06-18"));
    assert_eq!(lines[1]["id"], json!(2));
    let text = lines[1]["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("echoed"));
}

#[tokio::test]
async fn prompts_capability_advertised_only_with_a_provider() {
    let with = server_with_prompts()
        .handle(req(1, "initialize", json!({})))
        .await
        .unwrap();
    assert!(
        with.result.unwrap()["capabilities"]["prompts"].is_object(),
        "advertised"
    );
    let without = server(None)
        .handle(req(1, "initialize", json!({})))
        .await
        .unwrap();
    assert!(
        without.result.unwrap()["capabilities"]
            .get("prompts")
            .is_none(),
        "not advertised"
    );
}

#[tokio::test]
async fn prompts_list_and_get_expose_skills() {
    let s = server_with_prompts();
    let list = s.handle(req(2, "prompts/list", json!({}))).await.unwrap();
    let list = list.result.unwrap();
    let names: Vec<&str> = list["prompts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["summarize"]);

    let got = s
        .handle(req(3, "prompts/get", json!({ "name": "summarize" })))
        .await
        .unwrap();
    let result = got.result.unwrap();
    assert_eq!(result["description"], json!("Summarize text"));
    let msg = &result["messages"][0];
    assert_eq!(msg["role"], json!("user"));
    assert!(msg["content"]["text"]
        .as_str()
        .unwrap()
        .contains("concisely"));
}

#[tokio::test]
async fn prompts_get_unknown_or_no_provider_errors() {
    // Unknown prompt on a provider-backed server → INVALID_PARAMS.
    let unknown = server_with_prompts()
        .handle(req(4, "prompts/get", json!({ "name": "nope" })))
        .await
        .unwrap();
    assert_eq!(unknown.error.unwrap().code, -32602);
    // No provider → prompts/list is empty and prompts/get is METHOD_NOT_FOUND.
    let s = server(None);
    let empty = s.handle(req(5, "prompts/list", json!({}))).await.unwrap();
    let empty = empty.result.unwrap();
    assert!(empty["prompts"].as_array().unwrap().is_empty());
    let nm = s
        .handle(req(6, "prompts/get", json!({ "name": "summarize" })))
        .await
        .unwrap();
    assert_eq!(nm.error.unwrap().code, -32601);
}

#[tokio::test]
async fn resources_capability_advertised_only_with_a_provider() {
    let with = server_with_resources()
        .handle(req(1, "initialize", json!({})))
        .await
        .unwrap();
    assert!(
        with.result.unwrap()["capabilities"]["resources"].is_object(),
        "advertised"
    );
    let without = server(None)
        .handle(req(1, "initialize", json!({})))
        .await
        .unwrap();
    assert!(
        without.result.unwrap()["capabilities"]
            .get("resources")
            .is_none(),
        "not advertised"
    );
}

#[tokio::test]
async fn resources_list_and_read_expose_views() {
    let s = server_with_resources();
    let list = s.handle(req(2, "resources/list", json!({}))).await.unwrap();
    let list = list.result.unwrap();
    let uris: Vec<&str> = list["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["uri"].as_str().unwrap())
        .collect();
    assert_eq!(uris, vec!["catalerum://notes"]);

    let got = s
        .handle(req(
            3,
            "resources/read",
            json!({ "uri": "catalerum://notes" }),
        ))
        .await
        .unwrap();
    let result = got.result.unwrap();
    let content = &result["contents"][0];
    assert_eq!(content["uri"], json!("catalerum://notes"));
    assert_eq!(content["mimeType"], json!("text/markdown"));
    assert!(content["text"].as_str().unwrap().contains("hello"));
}

#[tokio::test]
async fn resources_read_unknown_or_no_provider_errors() {
    // Unknown uri on a provider-backed server → INVALID_PARAMS.
    let unknown = server_with_resources()
        .handle(req(
            4,
            "resources/read",
            json!({ "uri": "catalerum://nope" }),
        ))
        .await
        .unwrap();
    assert_eq!(unknown.error.unwrap().code, -32602);
    // No provider → resources/list empty and resources/read METHOD_NOT_FOUND.
    let s = server(None);
    let empty = s.handle(req(5, "resources/list", json!({}))).await.unwrap();
    let empty = empty.result.unwrap();
    assert!(empty["resources"].as_array().unwrap().is_empty());
    let nm = s
        .handle(req(
            6,
            "resources/read",
            json!({ "uri": "catalerum://notes" }),
        ))
        .await
        .unwrap();
    assert_eq!(nm.error.unwrap().code, -32601);
}
