//! JSON-RPC 2.0 envelope types for the MCP transport (SOUL §26).
//!
//! MCP speaks JSON-RPC 2.0 over a stream (stdio: one JSON object per line). A
//! message with an `id` is a **request** (expects a response); without one it is a
//! **notification** (no response). These are the minimal types the server needs;
//! the method-specific `params`/`result` payloads stay as opaque [`Value`]s shaped
//! by [`crate::server`].

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC: the request was not valid JSON.
pub const PARSE_ERROR: i64 = -32700;
/// JSON-RPC: the JSON was not a valid request object.
pub const INVALID_REQUEST: i64 = -32600;
/// JSON-RPC: the method does not exist.
pub const METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC: the params were invalid (e.g. an unknown tool, a missing field).
pub const INVALID_PARAMS: i64 = -32602;

/// An incoming JSON-RPC request or notification.
#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcRequest {
    /// Request id; absent (or null) → a notification (no response expected).
    #[serde(default)]
    pub id: Option<Value>,
    /// The method name (`initialize`, `tools/list`, `tools/call`, …).
    pub method: String,
    /// Method parameters, method-specific.
    #[serde(default)]
    pub params: Option<Value>,
}

impl JsonRpcRequest {
    /// Whether this is a notification (no `id`) — it must produce **no** response.
    #[must_use]
    pub fn is_notification(&self) -> bool {
        matches!(self.id, None | Some(Value::Null))
    }

    /// The client's **progress token** (`params._meta.progressToken`), set when the
    /// client asked for progress updates on this request per the MCP streamable-HTTP
    /// transport. `None` when the client did not opt into progress.
    #[must_use]
    pub fn progress_token(&self) -> Option<&Value> {
        self.params.as_ref()?.get("_meta")?.get("progressToken")
    }
}

/// An outgoing JSON-RPC response. Exactly one of `result`/`error` is set.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct JsonRpcResponse {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

/// An outgoing JSON-RPC **notification**: a message with `method`/`params` but no
/// `id`, so it expects no response. An MCP server pushes these to a client over a
/// stream — e.g. `notifications/progress` for a long-running `tools/call` on the
/// streamable-HTTP transport, or any unsolicited server→client message.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct JsonRpcNotification {
    pub jsonrpc: &'static str,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl JsonRpcNotification {
    /// A notification for `method` carrying `params`.
    #[must_use]
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            method: method.into(),
            params: Some(params),
        }
    }

    /// A standard MCP `notifications/progress` update against `progress_token` (the
    /// token the request supplied in `_meta.progressToken`). `progress` is the
    /// amount done so far (monotonically increasing across a request); `total` and
    /// `message` are optional per the spec and omitted from the wire when `None`.
    #[must_use]
    pub fn progress(
        progress_token: Value,
        progress: f64,
        total: Option<f64>,
        message: Option<String>,
    ) -> Self {
        let mut params = serde_json::json!({
            "progressToken": progress_token,
            "progress": progress,
        });
        if let Some(total) = total {
            params["total"] = serde_json::json!(total);
        }
        if let Some(message) = message {
            params["message"] = serde_json::json!(message);
        }
        Self::new("notifications/progress", params)
    }
}

/// A JSON-RPC error object.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpcResponse {
    /// A successful response carrying `result`.
    #[must_use]
    pub fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    /// An error response with `code` + `message`.
    #[must_use]
    pub fn error(id: Value, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn progress_token_reads_meta_field_when_present() {
        let req: JsonRpcRequest = serde_json::from_value(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "slow", "_meta": { "progressToken": "abc" } },
        }))
        .unwrap();
        assert_eq!(req.progress_token(), Some(&json!("abc")));
    }

    #[test]
    fn progress_token_is_none_without_meta_or_params() {
        let no_meta: JsonRpcRequest = serde_json::from_value(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "x" },
        }))
        .unwrap();
        assert_eq!(no_meta.progress_token(), None);
        let no_params: JsonRpcRequest =
            serde_json::from_value(json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" })).unwrap();
        assert_eq!(no_params.progress_token(), None);
    }

    #[test]
    fn progress_notification_has_the_mcp_shape() {
        let n = JsonRpcNotification::progress(json!("tok"), 0.5, Some(1.0), Some("half".into()));
        let v = serde_json::to_value(&n).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["method"], "notifications/progress");
        assert_eq!(v["params"]["progressToken"], "tok");
        assert_eq!(v["params"]["progress"], 0.5);
        assert_eq!(v["params"]["total"], 1.0);
        assert_eq!(v["params"]["message"], "half");
        // A notification carries no `id`.
        assert!(v.get("id").is_none());
    }

    #[test]
    fn progress_notification_omits_optional_fields_when_absent() {
        let n = JsonRpcNotification::progress(json!(7), 3.0, None, None);
        let v = serde_json::to_value(&n).unwrap();
        assert_eq!(v["params"]["progressToken"], 7);
        assert_eq!(v["params"]["progress"], 3.0);
        assert!(v["params"].get("total").is_none());
        assert!(v["params"].get("message").is_none());
    }
}
