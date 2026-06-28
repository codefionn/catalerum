//! MCP **client** — the inbound half of principle 15 (SOUL §26): catalerum
//! connects to *external* MCP servers (Playwright MCP, a filesystem server, a
//! hosted SaaS MCP, …) and folds their tools into the same scoped
//! [`ToolRegistry`] the agent loop, the API, and our own MCP server dispatch
//! against.
//!
//! Two transports, one tool surface:
//! - **stdio** ([`StdioMcpClient`]) — a child process speaking newline-delimited
//!   JSON-RPC on its stdin/stdout (the mirror of [`crate::transport`]).
//! - **HTTP / SSE** ([`crate::http_client`]) — the MCP "streamable HTTP"
//!   transport, with pluggable [`auth`](crate::auth) (bearer, custom header, or
//!   OAuth2/SSO).
//!
//! Both implement [`McpTransport`]; the handshake, `tools/list`, and `tools/call`
//! logic is shared and transport-agnostic. Each imported tool is registered as
//! `{server}_{tool}` and gated on `mcp:use@{server}` (SOUL §19) — a protected
//! scope no base role holds, so a remote tool is reachable only by an owner or an
//! explicitly granted agent, exactly like `run_command`. Deny-by-default.

use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use catalerum_core::capability::{Action, Capability, Resource};
use catalerum_core::error::{Error, Result};
use catalerum_core::tool::{Tool, ToolContext};

/// The MCP protocol version this client advertises (matches [`crate::server`]).
pub(crate) const PROTOCOL_VERSION: &str = "2025-06-18";
/// The `clientInfo.name` we send in the handshake.
const CLIENT_NAME: &str = "catalerum";

/// A bidirectional MCP channel that can issue JSON-RPC requests/notifications,
/// implemented by both the stdio and HTTP transports. `&self` (not `&mut`): each
/// transport owns its interior mutability (a mutex over the stdio pipes, an
/// `Arc`-cheap `reqwest::Client` for HTTP), so one connected client is shared
/// across every imported tool.
#[async_trait]
pub trait McpTransport: Send + Sync {
    /// Issue a JSON-RPC request and return its `result` (a JSON-RPC `error` maps
    /// to [`Error::Provider`]).
    async fn request(&self, method: &str, params: Value) -> Result<Value>;

    /// Issue a JSON-RPC notification (no id, no response expected).
    async fn notify(&self, method: &str, params: Value) -> Result<()>;
}

/// Interpret one JSON-RPC response object: a present `error` → [`Error::Provider`],
/// otherwise its `result` (or `null`). Shared by every transport.
pub(crate) fn jsonrpc_result(msg: &Value, method: &str) -> Result<Value> {
    if let Some(err) = msg.get("error") {
        let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
        let message = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        return Err(Error::provider(format!(
            "mcp: `{method}` failed ({code}): {message}"
        )));
    }
    Ok(msg.get("result").cloned().unwrap_or(Value::Null))
}

/// The `initialize` params every client sends.
pub(crate) fn initialize_params() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": { "name": CLIENT_NAME, "version": env!("CARGO_PKG_VERSION") },
    })
}

/// The MCP handshake over any transport: `initialize` (await), then the
/// `notifications/initialized` notification (fire-and-forget).
async fn handshake(t: &dyn McpTransport) -> Result<()> {
    t.request("initialize", initialize_params()).await?;
    t.notify("notifications/initialized", json!({})).await
}

/// `tools/list` → the server's tools as [`RemoteTool`]s (descriptors that fail to
/// parse are skipped, not fatal).
async fn list_remote_tools(t: &dyn McpTransport) -> Result<Vec<RemoteTool>> {
    let result = t.request("tools/list", json!({})).await?;
    let tools = result
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok(tools
        .iter()
        .filter_map(RemoteTool::from_descriptor)
        .collect())
}

/// `tools/call` → the raw MCP result object (`{ content, isError, … }`), returned
/// verbatim so nothing the server emitted is lost.
async fn call_remote_tool(t: &dyn McpTransport, name: &str, arguments: Value) -> Result<Value> {
    t.request(
        "tools/call",
        json!({ "name": name, "arguments": arguments }),
    )
    .await
}

// ---------------------------------------------------------------------------
// stdio transport
// ---------------------------------------------------------------------------

/// A JSON-RPC 2.0 framing over a line-delimited duplex stream — the MCP stdio
/// transport, the client mirror of [`crate::transport::serve`]. Generic over the
/// writer/reader so it drives a real child process in the binary and an in-memory
/// pipe in tests.
struct Connection<W, R> {
    writer: W,
    reader: Lines<R>,
    next_id: u64,
}

impl<W, R> Connection<W, R>
where
    W: AsyncWrite + Unpin,
    R: AsyncBufRead + Unpin,
{
    fn new(writer: W, reader: R) -> Self {
        Self {
            writer,
            reader: reader.lines(),
            next_id: 0,
        }
    }

    /// Send a request and await the response carrying our id, skipping interleaved
    /// notifications and non-JSON log lines the server may emit on stdout. EOF
    /// before the reply means the server died.
    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        self.write_line(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await?;
        loop {
            let Some(raw) = self
                .reader
                .next_line()
                .await
                .map_err(|e| Error::provider(format!("mcp: read failed: {e}")))?
            else {
                return Err(Error::provider(format!(
                    "mcp: server closed the stream before answering `{method}`"
                )));
            };
            if raw.trim().is_empty() {
                continue;
            }
            let Ok(msg) = serde_json::from_str::<Value>(&raw) else {
                continue;
            };
            if msg.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            return jsonrpc_result(&msg, method);
        }
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.write_line(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await
    }

    /// Write one JSON value as a single newline-terminated line, flushed.
    async fn write_line(&mut self, value: &Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(value)
            .map_err(|e| Error::provider(format!("mcp: encode failed: {e}")))?;
        bytes.push(b'\n');
        self.writer
            .write_all(&bytes)
            .await
            .map_err(|e| Error::provider(format!("mcp: write failed: {e}")))?;
        self.writer
            .flush()
            .await
            .map_err(|e| Error::provider(format!("mcp: flush failed: {e}")))
    }
}

/// A [`Connection`] behind a mutex so it satisfies [`McpTransport`]'s `&self`
/// contract (calls serialize — the JSON-RPC framing is one-request/one-response,
/// and a single child server is sequential anyway). Reused by [`StdioMcpClient`]
/// and the in-memory loopback tests.
struct ConnTransport<W, R> {
    conn: Mutex<Connection<W, R>>,
}

impl<W, R> ConnTransport<W, R>
where
    W: AsyncWrite + Unpin,
    R: AsyncBufRead + Unpin,
{
    fn new(writer: W, reader: R) -> Self {
        Self {
            conn: Mutex::new(Connection::new(writer, reader)),
        }
    }
}

#[async_trait]
impl<W, R> McpTransport for ConnTransport<W, R>
where
    W: AsyncWrite + Unpin + Send,
    R: AsyncBufRead + Unpin + Send,
{
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.conn.lock().await.request(method, params).await
    }
    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.conn.lock().await.notify(method, params).await
    }
}

/// A connected stdio MCP server: owns the child process (reaped on drop via
/// `kill_on_drop`) and a [`ConnTransport`] over its pipes.
pub struct StdioMcpClient {
    // Kept alive so the child isn't reaped while we still hold its pipes.
    _child: Child,
    inner: ConnTransport<ChildStdin, BufReader<ChildStdout>>,
}

impl StdioMcpClient {
    /// Spawn `command args` (with extra `env`) as an MCP server. The child's
    /// stderr is inherited so its own logs reach ours. The handshake is performed
    /// separately by [`load_server_tools`].
    ///
    /// # Errors
    /// If the process can't be spawned or its pipes are missing.
    pub async fn connect(
        server: &str,
        command: &str,
        args: &[String],
        env: &[(String, String)],
    ) -> Result<Self> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().map_err(|e| {
            Error::provider(format!(
                "mcp: failed to spawn `{command}` for server `{server}`: {e}"
            ))
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::provider("mcp: spawned child has no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::provider("mcp: spawned child has no stdout"))?;
        Ok(Self {
            _child: child,
            inner: ConnTransport::new(stdin, BufReader::new(stdout)),
        })
    }
}

#[async_trait]
impl McpTransport for StdioMcpClient {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.inner.request(method, params).await
    }
    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.inner.notify(method, params).await
    }
}

// ---------------------------------------------------------------------------
// shared: remote tools + registration
// ---------------------------------------------------------------------------

/// A tool advertised by a remote MCP server (one entry of `tools/list`).
#[derive(Clone, Debug, PartialEq)]
struct RemoteTool {
    name: String,
    description: String,
    input_schema: Value,
}

impl RemoteTool {
    /// Parse one `tools/list` descriptor; `None` if it has no `name`.
    fn from_descriptor(v: &Value) -> Option<Self> {
        let name = v.get("name").and_then(Value::as_str)?.to_string();
        Some(Self {
            name,
            description: v
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            input_schema: v
                .get("inputSchema")
                .cloned()
                .unwrap_or_else(|| json!({ "type": "object" })),
        })
    }
}

/// A single remote MCP tool, adapted to catalerum's [`Tool`] trait so it lives in
/// the shared registry. The registered name is prefixed (`{server}_{tool}`) to
/// avoid collisions; calls go out under the original server-side name.
struct RemoteMcpTool {
    transport: Arc<dyn McpTransport>,
    registered_name: String,
    remote_name: String,
    description: String,
    schema: Value,
    capability: Capability,
}

#[async_trait]
impl Tool for RemoteMcpTool {
    fn name(&self) -> &str {
        &self.registered_name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn required_capability(&self) -> Option<Capability> {
        Some(self.capability.clone())
    }
    fn parameters_schema(&self) -> Value {
        self.schema.clone()
    }
    async fn invoke(&self, args: Value, _ctx: &ToolContext) -> Result<Value> {
        call_remote_tool(self.transport.as_ref(), &self.remote_name, args).await
    }
}

/// Handshake a connected `transport`, list its tools, and wrap each as a [`Tool`]
/// gated on `mcp:use@{server}`. Shared by the stdio and HTTP loaders.
pub(crate) async fn import_tools(
    transport: Arc<dyn McpTransport>,
    server: &str,
    allow: &[String],
) -> Result<Vec<Arc<dyn Tool>>> {
    handshake(transport.as_ref()).await?;
    let remote = list_remote_tools(transport.as_ref()).await?;
    // One capability per server: `mcp:use@{server}`. A grant can widen to the
    // whole domain (`mcp:use`) or scope to a single server's selector (§19).
    let capability = Capability::new(Action::Use, Resource::new("mcp", server));
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    for rt in remote {
        if !allow.is_empty() && !allow.iter().any(|a| a == &rt.name) {
            continue;
        }
        tools.push(Arc::new(RemoteMcpTool {
            transport: transport.clone(),
            registered_name: sanitize_tool_name(&format!("{server}_{}", rt.name)),
            remote_name: rt.name,
            description: rt.description,
            schema: rt.input_schema,
            capability: capability.clone(),
        }));
    }
    Ok(tools)
}

/// Connect to a **stdio** MCP server and return its tools, ready to register into
/// the §7 [`ToolRegistry`](catalerum_core::tool::ToolRegistry).
///
/// # Errors
/// If the server can't be spawned or the handshake / `tools/list` fails. (The
/// caller logs and skips a failing server so one bad entry never blocks boot.)
pub async fn load_server_tools(
    server: &str,
    command: &str,
    args: &[String],
    env: &[(String, String)],
    allow: &[String],
) -> Result<Vec<Arc<dyn Tool>>> {
    let client: Arc<dyn McpTransport> =
        Arc::new(StdioMcpClient::connect(server, command, args, env).await?);
    import_tools(client, server, allow).await
}

/// Coerce a name to the LLM function-name charset (`[A-Za-z0-9_-]`, ≤ 64 chars):
/// any other byte becomes `_`. Remote servers occasionally use `.`/`/` in tool
/// names, which OpenAI-style tool calling rejects.
fn sanitize_tool_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    out.truncate(64);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use catalerum_core::tool::ToolRegistry;
    use tokio::io::{duplex, split, BufReader};

    use crate::server::McpServer;

    /// A trivial registry tool the loopback server exposes — echoes its args.
    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "returns its args"
        }
        fn parameters_schema(&self) -> Value {
            json!({ "type": "object", "properties": { "x": { "type": "integer" } } })
        }
        async fn invoke(&self, args: Value, _ctx: &ToolContext) -> Result<Value> {
            Ok(args)
        }
    }

    /// A [`ConnTransport`] wired to catalerum's *own* [`McpServer`] over an
    /// in-memory duplex — exercises the real JSON-RPC framing/handshake without a
    /// child process.
    fn loopback() -> impl McpTransport {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(EchoTool));
        let server = McpServer::new(registry, ToolContext::default());

        let (client_side, server_side) = duplex(64 * 1024);
        let (client_r, client_w) = split(client_side);
        let (server_r, server_w) = split(server_side);
        tokio::spawn(async move {
            let _ = crate::transport::serve(&server, BufReader::new(server_r), server_w).await;
        });
        ConnTransport::new(client_w, BufReader::new(client_r))
    }

    #[tokio::test]
    async fn handshake_then_list_and_call_roundtrip() {
        let t = loopback();
        handshake(&t).await.expect("handshake");

        // tools/list surfaces the echo tool with its input schema.
        let tools = list_remote_tools(&t).await.expect("list");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        assert_eq!(tools[0].input_schema["type"], json!("object"));

        // tools/call dispatches and returns the MCP content envelope.
        let out = call_remote_tool(&t, "echo", json!({ "x": 7 }))
            .await
            .expect("call");
        assert_eq!(out["isError"], json!(false));
        let text = out["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("\"x\":7"), "echoed args in content: {text}");
    }

    #[tokio::test]
    async fn unknown_tool_surfaces_as_jsonrpc_error() {
        let t = loopback();
        handshake(&t).await.expect("handshake");
        // The server reports an unknown tool as INVALID_PARAMS → mapped to Provider.
        let err = call_remote_tool(&t, "nope", json!({})).await.unwrap_err();
        assert!(matches!(err, Error::Provider(_)), "got {err:?}");
    }

    #[test]
    fn remote_tool_descriptor_parsing_and_defaults() {
        let full = RemoteTool::from_descriptor(&json!({
            "name": "browser_navigate",
            "description": "go to a url",
            "inputSchema": { "type": "object", "properties": { "url": { "type": "string" } } },
        }))
        .unwrap();
        assert_eq!(full.name, "browser_navigate");
        assert_eq!(full.description, "go to a url");
        assert_eq!(
            full.input_schema["properties"]["url"]["type"],
            json!("string")
        );

        let bare = RemoteTool::from_descriptor(&json!({ "name": "x" })).unwrap();
        assert_eq!(bare.description, "");
        assert_eq!(bare.input_schema, json!({ "type": "object" }));
        assert!(RemoteTool::from_descriptor(&json!({ "description": "no name" })).is_none());
    }

    #[test]
    fn sanitize_keeps_valid_chars_and_replaces_the_rest() {
        assert_eq!(
            sanitize_tool_name("playwright_browser_navigate"),
            "playwright_browser_navigate"
        );
        assert_eq!(sanitize_tool_name("fs.read/file"), "fs_read_file");
        assert_eq!(sanitize_tool_name("a-b_1"), "a-b_1");
        assert_eq!(sanitize_tool_name(&"x".repeat(100)).len(), 64);
    }

    #[test]
    fn jsonrpc_result_unwraps_result_or_maps_error() {
        assert_eq!(
            jsonrpc_result(&json!({ "id": 1, "result": { "ok": true } }), "m").unwrap(),
            json!({ "ok": true })
        );
        let err = jsonrpc_result(
            &json!({ "id": 1, "error": { "code": -32601, "message": "nope" } }),
            "m",
        )
        .unwrap_err();
        assert!(matches!(err, Error::Provider(_)), "got {err:?}");
    }
}
