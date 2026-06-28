//! The live external-MCP-server manager (SOUL §26).
//!
//! Connects to a persisted [`McpServerDef`] as an MCP **client** (stdio or
//! HTTP/SSE) and folds its tools into the §7 registry's **runtime overlay**
//! ([`ToolRegistry::register_dynamic`]), so a server created/edited by the
//! `*_mcp_server` tools is usable **in the same session, no restart** — and a
//! deleted one's tools disappear at once. The manager tracks which tool names it
//! registered per server so it can cleanly disconnect/reconnect.
//!
//! It changes nothing about authority: each imported tool still carries its own
//! `mcp:use@{server}` gate (§19), so hot-plugging a server grants no one access
//! they didn't already have.

use std::collections::HashMap;
use std::sync::{Arc, RwLock, RwLockWriteGuard};

use catalerum_core::error::Result;
use catalerum_core::model::{McpAuthSpec, McpServerDef};
use catalerum_core::tool::ToolRegistry;

/// Connects external MCP servers and registers their tools into a shared
/// [`ToolRegistry`] overlay. Cheap to clone (the registry shares its overlay
/// `Arc`; the connection map is `Arc`-backed).
#[derive(Clone)]
pub struct McpManager {
    registry: ToolRegistry,
    /// Per server name → the overlay tool names it currently contributes, so a
    /// disconnect/reconnect removes exactly its tools.
    connected: Arc<RwLock<HashMap<String, Vec<String>>>>,
}

impl McpManager {
    /// A manager that registers into `registry`'s runtime overlay.
    #[must_use]
    pub fn new(registry: ToolRegistry) -> Self {
        Self {
            registry,
            connected: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Connect (or **reconnect**) `def`: drop any tools a same-named server
    /// previously contributed, then import this definition's tools into the
    /// overlay. Returns the number of tools imported.
    ///
    /// # Errors
    /// If the server can't be spawned/reached or its `tools/list` fails — the
    /// caller (a tool or the boot loader) surfaces or logs it.
    pub async fn connect(&self, def: &McpServerDef) -> Result<usize> {
        // Reconnect semantics: remove the old tool set before importing the new.
        self.disconnect(&def.name);
        let tools = if def.is_http() {
            catalerum_mcp::load_http_server_tools(
                &def.name,
                &def.url,
                build_auth(&def.auth),
                &def.tools,
            )
            .await?
        } else {
            let env: Vec<(String, String)> = def
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            catalerum_mcp::load_server_tools(&def.name, &def.command, &def.args, &env, &def.tools)
                .await?
        };
        let mut names = Vec::with_capacity(tools.len());
        for tool in tools {
            names.push(tool.name().to_string());
            self.registry.register_dynamic(tool);
        }
        let count = names.len();
        self.connected().insert(def.name.clone(), names);
        Ok(count)
    }

    /// Disconnect a server by name: remove every overlay tool it contributed.
    /// Returns how many were removed (0 if it wasn't connected). Sync — no I/O;
    /// dropping the tool `Arc`s reaps a stdio child / closes an HTTP client.
    pub fn disconnect(&self, name: &str) -> usize {
        let removed = self.connected().remove(name).unwrap_or_default();
        for tool in &removed {
            self.registry.unregister_dynamic(tool);
        }
        removed.len()
    }

    /// Whether a server with `name` is currently connected.
    #[must_use]
    pub fn is_connected(&self, name: &str) -> bool {
        self.connected().contains_key(name)
    }

    /// Write guard over the connection map (recovers from a poisoned lock).
    fn connected(&self) -> RwLockWriteGuard<'_, HashMap<String, Vec<String>>> {
        self.connected.write().unwrap_or_else(|e| e.into_inner())
    }
}

/// Map a stored [`McpAuthSpec`] to an HTTP-MCP auth provider (SOUL §26). Mirrors
/// the binary's config-file `build_mcp_auth`, but from the DB-stored spec.
fn build_auth(spec: &McpAuthSpec) -> Arc<dyn catalerum_mcp::AuthProvider> {
    match spec.kind.trim().to_ascii_lowercase().as_str() {
        "bearer" => catalerum_mcp::auth::bearer(spec.token.clone()),
        "header" => {
            catalerum_mcp::auth::header(spec.header_name.clone(), spec.header_value.clone())
        }
        "oauth2" => catalerum_mcp::auth::oauth2(catalerum_mcp::OAuth2Params {
            token_url: spec.token_url.clone(),
            grant_type: if spec.grant_type.trim().is_empty() {
                "client_credentials".to_string()
            } else {
                spec.grant_type.clone()
            },
            client_id: spec.client_id.clone(),
            client_secret: spec.client_secret.clone(),
            refresh_token: spec.refresh_token.clone(),
            scope: spec.scope.clone(),
        }),
        _ => catalerum_mcp::auth::none(),
    }
}
