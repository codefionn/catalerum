//! MCP **resources** (SOUL §26): read views over the workspace — notes, tasks,
//! calendar — that an external agent can list and read for context (distinct from
//! tools, which *act*). The provider is abstracted so `catalerum-mcp` stays
//! core-only; the binary wires a concrete store-backed one.

use async_trait::async_trait;

/// A source of MCP resources (read views), scoped to the same workspace as the
/// [`McpServer`](crate::McpServer)'s tool context.
#[async_trait]
pub trait ResourceProvider: Send + Sync {
    /// List the available resources (each a stable `uri` + metadata).
    async fn list(&self) -> Vec<ResourceInfo>;

    /// Read one resource by `uri`, or `None` if it is unknown.
    async fn read(&self, uri: &str) -> Option<ResourceContent>;
}

/// A resource's listing entry (`resources/list`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceInfo {
    pub uri: String,
    pub name: String,
    pub description: String,
    pub mime_type: String,
}

/// A resource's content (`resources/read`): the `uri` echoed, the MIME type, and
/// the text body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceContent {
    pub uri: String,
    pub mime_type: String,
    pub text: String,
}
