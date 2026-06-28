//! MCP **prompts** (SOUL §26): catalerum exposes its skills (§23) as MCP prompts,
//! so an external agent can pull a packaged runbook by name.
//!
//! The provider is abstracted so `catalerum-mcp` stays core-only (no store dep):
//! the binary wires a concrete [`PromptProvider`] backed by the workspace's skills.

use async_trait::async_trait;

/// A source of MCP prompts (e.g. the workspace's skills). Scoped to the same
/// workspace as the [`McpServer`](crate::McpServer)'s tool context.
#[async_trait]
pub trait PromptProvider: Send + Sync {
    /// List the available prompts (name + one-line description).
    async fn list(&self) -> Vec<PromptInfo>;

    /// Fetch one prompt's content by name, or `None` if unknown.
    async fn get(&self, name: &str) -> Option<PromptContent>;
}

/// A prompt's listing entry (`prompts/list`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptInfo {
    pub name: String,
    pub description: String,
}

/// A prompt's content (`prompts/get`): an optional description plus the body,
/// which is rendered as a single user message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptContent {
    pub description: Option<String>,
    pub text: String,
}
