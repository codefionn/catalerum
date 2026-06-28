//! catalerum-mcp — external MCP server exposing catalerum's scoped tools,
//! resources, and prompts so Claude Code / Codex / opencode are first-class
//! clients under the same workspace + capability scoping (SOUL §26, principle 15).
//!
//! JSON-RPC 2.0 ([`protocol`]); the [`McpServer`] handling `initialize` /
//! `tools/list` / `tools/call` (§7 registry under a scoped
//! [`ToolContext`](catalerum_core::tool::ToolContext)) / `prompts/list` /
//! `prompts/get` (skills §23 via a [`PromptProvider`]) / `resources/list` /
//! `resources/read` (read views via a [`ResourceProvider`]) / `ping`; a stdio
//! [`serve`] transport; and the streamable-HTTP streaming primitives ([`sse`]:
//! [`sse_frame`] + [`SessionHub`], driven over axum by `catalerum-api`). The
//! service-token→grant resolution is a later slice.
//!
//! The [`client`] module is the inbound half (principle 15): catalerum as an MCP
//! *client* of external servers (Playwright MCP, …), folding their tools into the
//! same scoped registry under `mcp:use@{server}` (SOUL §19/§26). It speaks both
//! **stdio** ([`client::StdioMcpClient`]) and **HTTP/SSE** ([`http_client`]); the
//! HTTP transport authenticates via a pluggable [`auth`] provider (bearer / header
//! / OAuth2-SSO).

pub mod auth;
pub mod client;
pub mod http_client;
pub mod prompts;
pub mod protocol;
pub mod resources;
pub mod server;
pub mod sse;
pub mod transport;

pub use auth::{AuthProvider, OAuth2Params};
pub use client::{load_server_tools, McpTransport, StdioMcpClient};
pub use http_client::{load_http_server_tools, HttpMcpClient};
pub use prompts::{PromptContent, PromptInfo, PromptProvider};
pub use protocol::{JsonRpcError, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse};
pub use resources::{ResourceContent, ResourceInfo, ResourceProvider};
pub use server::McpServer;
pub use sse::{sse_frame, SessionHub};
pub use transport::serve;
