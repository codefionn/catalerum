//! The HTTP / SSE MCP client transport (SOUL §26): the MCP **streamable HTTP**
//! transport, the client mirror of catalerum's own `POST /mcp` server route.
//!
//! Each JSON-RPC request is an HTTP `POST` to a single endpoint with
//! `Accept: application/json, text/event-stream`. The server replies with either
//! a plain `application/json` body (one JSON-RPC response) or a `text/event-stream`
//! SSE body whose `data:` event carries the response — this transport handles
//! both. A `Mcp-Session-Id` returned by the server (typically on `initialize`) is
//! captured and echoed on every later request, and `MCP-Protocol-Version` is sent
//! throughout. Authentication is delegated to a pluggable
//! [`AuthProvider`](crate::auth::AuthProvider) (bearer / header / OAuth2-SSO).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use catalerum_core::error::{Error, Result};
use catalerum_core::tool::Tool;

use crate::auth::AuthProvider;
use crate::client::{import_tools, jsonrpc_result, McpTransport, PROTOCOL_VERSION};

/// A connected HTTP/SSE MCP server. Cheap to share (the `reqwest::Client` is
/// `Arc`-backed); calls are stateless apart from the captured session id.
pub struct HttpMcpClient {
    http: reqwest::Client,
    url: String,
    auth: Arc<dyn AuthProvider>,
    /// The server's `Mcp-Session-Id`, echoed on subsequent requests once known.
    session_id: Mutex<Option<String>>,
    next_id: AtomicU64,
}

impl HttpMcpClient {
    /// Build a client for `url` authenticated by `auth`. The handshake is run
    /// separately by [`load_http_server_tools`].
    ///
    /// # Errors
    /// If the underlying HTTP client can't be constructed.
    pub fn new(url: impl Into<String>, auth: Arc<dyn AuthProvider>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .build()
            .map_err(|e| Error::provider(format!("mcp http: client build failed: {e}")))?;
        Ok(Self {
            http,
            url: url.into(),
            auth,
            session_id: Mutex::new(None),
            next_id: AtomicU64::new(0),
        })
    }

    /// POST a JSON-RPC envelope with the negotiated headers, auth, and session id.
    async fn send(&self, body: &Value) -> Result<reqwest::Response> {
        let mut req = self
            .http
            .post(&self.url)
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", PROTOCOL_VERSION)
            .json(body);
        for (k, v) in self.auth.headers().await? {
            req = req.header(k, v);
        }
        if let Some(sid) = self.session_id.lock().await.clone() {
            req = req.header("Mcp-Session-Id", sid);
        }
        req.send()
            .await
            .map_err(|e| Error::provider(format!("mcp http: request to {} failed: {e}", self.url)))
    }

    /// Capture a `Mcp-Session-Id` response header if the server set one.
    async fn capture_session(&self, resp: &reqwest::Response) {
        if let Some(sid) = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
        {
            *self.session_id.lock().await = Some(sid);
        }
    }
}

#[async_trait]
impl McpTransport for HttpMcpClient {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let resp = self
            .send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await?;
        self.capture_session(&resp).await;
        let status = resp.status();
        let is_sse = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|c| c.contains("text/event-stream"));
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(Error::provider(format!(
                "mcp http: `{method}` returned {status}: {}",
                truncate(text.trim(), 300)
            )));
        }
        let msg = if is_sse {
            read_sse_response(resp, id).await?
        } else {
            resp.json::<Value>().await.map_err(|e| {
                Error::provider(format!("mcp http: `{method}` body was not JSON: {e}"))
            })?
        };
        jsonrpc_result(&msg, method)
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let resp = self
            .send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await?;
        // A notification gets `202 Accepted` with no body; still honor a session id.
        self.capture_session(&resp).await;
        Ok(())
    }
}

/// Connect to an **HTTP/SSE** MCP server and return its tools, ready to register
/// into the §7 [`ToolRegistry`](catalerum_core::tool::ToolRegistry).
///
/// # Errors
/// If the handshake / `tools/list` fails (bad URL, auth rejected, server down).
/// The caller logs and skips a failing server so one bad entry never blocks boot.
pub async fn load_http_server_tools(
    server: &str,
    url: &str,
    auth: Arc<dyn AuthProvider>,
    allow: &[String],
) -> Result<Vec<Arc<dyn Tool>>> {
    let client: Arc<dyn McpTransport> = Arc::new(HttpMcpClient::new(url, auth)?);
    import_tools(client, server, allow).await
}

/// Read an SSE body until the `data:` event whose JSON-RPC message carries `id`.
async fn read_sse_response(resp: reqwest::Response, id: u64) -> Result<Value> {
    use futures::StreamExt;
    let mut stream = resp.bytes_stream();
    let mut acc = SseAccumulator::default();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| Error::provider(format!("mcp sse: read failed: {e}")))?;
        if let Some(msg) = acc.push(&String::from_utf8_lossy(&chunk), id) {
            return Ok(msg);
        }
    }
    acc.finish(id)
        .ok_or_else(|| Error::provider("mcp sse: stream ended before the response arrived"))
}

/// Incrementally parses an SSE stream (`data:` fields, blank-line event
/// boundaries), surfacing the first event whose JSON-RPC `id` matches — robust to
/// chunk boundaries falling mid-line and to interleaved server notifications.
#[derive(Default)]
struct SseAccumulator {
    line_buf: String,
    data: String,
}

impl SseAccumulator {
    /// Feed a chunk; return a matching event's message if one completed.
    fn push(&mut self, chunk: &str, id: u64) -> Option<Value> {
        self.line_buf.push_str(chunk);
        while let Some(pos) = self.line_buf.find('\n') {
            let line = self.line_buf[..pos].trim_end_matches('\r').to_string();
            self.line_buf.drain(..=pos);
            if line.is_empty() {
                if let Some(m) = self.take_event(id) {
                    return Some(m);
                }
            } else if let Some(rest) = line.strip_prefix("data:") {
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                // A single optional leading space after the colon is stripped (SSE).
                self.data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
            }
            // Other SSE fields (event:, id:, retry:, `:` comments) are ignored.
        }
        None
    }

    /// Consume the accumulated event; `Some` only if it parses and matches `id`.
    fn take_event(&mut self, id: u64) -> Option<Value> {
        if self.data.is_empty() {
            return None;
        }
        let matched = serde_json::from_str::<Value>(&self.data)
            .ok()
            .filter(|m| m.get("id").and_then(Value::as_u64) == Some(id));
        self.data.clear();
        matched
    }

    /// Flush a trailing event that had no terminating blank line (stream EOF).
    fn finish(&mut self, id: u64) -> Option<Value> {
        self.take_event(id)
    }
}

/// Truncate `s` to at most `max` chars (char-safe), appending `…` when clipped.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex as StdMutex;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    use crate::auth;

    #[test]
    fn sse_accumulator_matches_across_chunk_boundaries_and_skips_other_events() {
        let mut acc = SseAccumulator::default();
        // A non-matching event (id 9) is consumed and skipped.
        assert!(acc.push("data: {\"id\":9,\"result\":{}}\n\n", 1).is_none());
        // The matching event arrives split across three chunks, mid-line.
        assert!(acc.push("data: {\"id\":1,\"resu", 1).is_none());
        assert!(acc.push("lt\":{\"ok\":true}}\n", 1).is_none());
        let msg = acc
            .push("\n", 1)
            .expect("event completes on the blank line");
        assert_eq!(msg["result"], json!({ "ok": true }));
    }

    #[test]
    fn sse_accumulator_finish_flushes_an_unterminated_event() {
        let mut acc = SseAccumulator::default();
        assert!(acc.push("data: {\"id\":1,\"result\":5}\n", 1).is_none());
        // No trailing blank line, but EOF flush still yields it.
        assert_eq!(acc.finish(1).unwrap()["result"], json!(5));
    }

    /// A one-shot mock MCP-over-HTTP server. For each request it parses the
    /// JSON-RPC `method` and replies via `handler(method, head) -> (content_type,
    /// body)`. Returns the bound URL.
    async fn mock_server<F>(handler: F) -> String
    where
        F: Fn(&str, &str) -> (&'static str, String) + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handler = Arc::new(handler);
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let handler = handler.clone();
                tokio::spawn(async move {
                    serve_one(socket, handler).await;
                });
            }
        });
        format!("http://{addr}/mcp")
    }

    async fn serve_one<F>(mut socket: TcpStream, handler: Arc<F>)
    where
        F: Fn(&str, &str) -> (&'static str, String) + Send + Sync + 'static,
    {
        let (head, body) = read_request(&mut socket).await;
        let method = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| v.get("method").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_default();
        let (content_type, resp_body) = handler(&method, &head);
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
            resp_body.len()
        );
        let _ = socket.write_all(resp.as_bytes()).await;
        let _ = socket.shutdown().await;
    }

    /// Read one HTTP/1.1 request, returning (head, body) using `Content-Length`.
    async fn read_request(socket: &mut TcpStream) -> (String, String) {
        let mut buf: Vec<u8> = Vec::new();
        let mut tmp = [0u8; 2048];
        while let Ok(n) = socket.read(&mut tmp).await {
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = find(&buf, b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..pos]).to_string();
                let want = content_length(&head);
                let body_start = pos + 4;
                while buf.len() - body_start < want {
                    let Ok(n) = socket.read(&mut tmp).await else {
                        break;
                    };
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                let end = (body_start + want).min(buf.len());
                let body = String::from_utf8_lossy(&buf[body_start..end]).to_string();
                return (head, body);
            }
        }
        (String::from_utf8_lossy(&buf).to_string(), String::new())
    }

    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    fn content_length(head: &str) -> usize {
        head.lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.trim()
                    .eq_ignore_ascii_case("content-length")
                    .then(|| v.trim().parse().ok())
                    .flatten()
            })
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn http_json_request_applies_bearer_auth_and_parses_result() {
        let seen = Arc::new(StdMutex::new(None::<String>));
        let seen2 = seen.clone();
        let url = mock_server(move |method, head| {
            // Record the Authorization header the client sent.
            let authz = head.lines().find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.trim()
                    .eq_ignore_ascii_case("authorization")
                    .then(|| v.trim().to_string())
            });
            *seen2.lock().unwrap() = authz;
            assert_eq!(method, "ping");
            (
                "application/json",
                json!({ "jsonrpc": "2.0", "id": 1, "result": { "pong": true } }).to_string(),
            )
        })
        .await;

        let client = HttpMcpClient::new(url, auth::bearer("sekret")).unwrap();
        let out = client.request("ping", json!({})).await.unwrap();
        assert_eq!(out, json!({ "pong": true }));
        assert_eq!(seen.lock().unwrap().as_deref(), Some("Bearer sekret"));
    }

    #[tokio::test]
    async fn http_sse_response_is_parsed_end_to_end() {
        // The server answers with a single-event SSE stream (catalerum's own
        // `POST /mcp` shape when the client sends `Accept: text/event-stream`).
        let url = mock_server(|_method, _head| {
            let payload = json!({ "jsonrpc": "2.0", "id": 1, "result": { "via": "sse" } });
            ("text/event-stream", format!("data: {payload}\n\n"))
        })
        .await;

        let client = HttpMcpClient::new(url, auth::none()).unwrap();
        let out = client.request("tools/list", json!({})).await.unwrap();
        assert_eq!(out, json!({ "via": "sse" }));
    }
}
