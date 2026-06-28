//! Management REST for external MCP servers — catalerum as an MCP *client*
//! (SOUL §26).
//!
//! A workspace registers the external MCP servers it connects *out* to here
//! (stdio: spawn a command; http: connect to a URL with optional
//! bearer/header/oauth2 auth). Each enabled server's tools are folded into the
//! §7 registry as `{name}_{tool}`, each gated on `mcp:use@{name}` (§19), and
//! (re)connected live through the [`McpManager`] — no restart. This is the web
//! surface for the same store the `*_mcp_server` agent tools drive.
//!
//! **Reads** (list) gate on `mcp:read` — every role, since a member's tools
//! legitimately use a connected server. **Lifecycle** (create / update /
//! delete) is a workspace-operational config write — it stores workspace-shared
//! credentials, spawns processes / reaches external URLs, and injects tools the
//! whole workspace then sees — so it additionally requires a workspace
//! **administrator** (Owner/Admin) via [`Auth::require_workspace_admin`],
//! mirroring the external-DB connection routes.
//!
//! - `GET    /mcp-servers`        — list the workspace's servers (secrets redacted)
//! - `POST   /mcp-servers`        — create one and connect it live
//! - `PUT    /mcp-servers/{name}` — replace one by name and reconnect it
//! - `DELETE /mcp-servers/{name}` — disconnect and remove one
//!
//! Secrets never cross the wire: a server view reports only *whether* each
//! secret / env value is set, and an update with a blank secret keeps the stored
//! one (so editing a URL never forces re-typing a token) — unless the transport
//! or auth kind changed, where the old secrets no longer apply. `name` is the
//! server's identity: an update targets the path name and does not rename (a
//! rename is a create + delete, matching the `edit_mcp_server` tool).

use std::collections::BTreeMap;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use catalerum_core::capability::Action;
use catalerum_core::model::{McpAuthSpec, McpServerDef};
use catalerum_store::NewMcpServerDef;

use crate::auth::Auth;
use crate::error::{ApiError, ApiResult};
use crate::mcp_manager::McpManager;
use crate::state::AppState;

/// Mount the management routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/mcp-servers", get(list).post(create))
        .route(
            "/mcp-servers/{name}",
            axum::routing::put(update).delete(delete),
        )
}

fn default_true() -> bool {
    true
}

/// Create/update body — the full server definition. Secrets (`auth.*` tokens and
/// `env` values) are only carried when (re)entered; a blank field on update keeps
/// the stored value.
#[derive(Debug, Default, Deserialize)]
pub struct McpServerBody {
    /// Workspace-unique name; prefixes the server's tools and scopes
    /// `mcp:use@{name}`. Ignored on update (the path name is the identity).
    #[serde(default)]
    pub name: String,
    /// `"stdio"` (default) or `"http"`.
    #[serde(default)]
    pub transport: String,
    /// Program to spawn (stdio).
    #[serde(default)]
    pub command: String,
    /// Arguments to `command` (stdio).
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment for the child (stdio). Values are secret-preserving.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Endpoint URL (http).
    #[serde(default)]
    pub url: String,
    /// HTTP auth.
    #[serde(default)]
    pub auth: McpAuthBody,
    /// Whether to connect this server (default true).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Optional allow-list of remote tool names to import; empty → import all.
    #[serde(default)]
    pub tools: Vec<String>,
}

/// Mirrors [`McpAuthSpec`] on the wire. Secret fields (`token`, `header_value`,
/// `client_secret`, `refresh_token`) are only sent when set/changed.
#[derive(Debug, Default, Deserialize)]
pub struct McpAuthBody {
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub header_name: String,
    #[serde(default)]
    pub header_value: String,
    #[serde(default)]
    pub token_url: String,
    #[serde(default)]
    pub grant_type: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub client_secret: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub scope: String,
}

impl McpAuthBody {
    fn into_spec(self) -> McpAuthSpec {
        McpAuthSpec {
            kind: self.kind,
            token: self.token,
            header_name: self.header_name,
            header_value: self.header_value,
            token_url: self.token_url,
            grant_type: self.grant_type,
            client_id: self.client_id,
            client_secret: self.client_secret,
            refresh_token: self.refresh_token,
            scope: self.scope,
        }
    }
}

/// A redacted, edit-form-ready view of a stored server: secrets are never echoed
/// (only whether they are configured); `env` shows keys, not values; non-secret
/// auth fields (header name, oauth2 endpoint/client/scope) are echoed so the form
/// can prefill them.
#[derive(Debug, Serialize)]
pub struct McpServerView {
    pub name: String,
    pub transport: String,
    pub command: String,
    pub args: Vec<String>,
    /// The `env` keys (values redacted).
    pub env_keys: Vec<String>,
    pub url: String,
    pub auth: McpAuthView,
    pub enabled: bool,
    pub tools: Vec<String>,
    /// Whether the server is currently connected live.
    pub connected: bool,
    /// The most recent connect error, if the last (re)connect failed. The
    /// definition is persisted regardless and retries on the next boot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connect_error: Option<String>,
}

/// Redacted auth view — non-secret fields plus a bool per secret.
#[derive(Debug, Serialize)]
pub struct McpAuthView {
    pub kind: String,
    pub header_name: String,
    pub token_url: String,
    pub grant_type: String,
    pub client_id: String,
    pub scope: String,
    pub has_token: bool,
    pub has_header_value: bool,
    pub has_client_secret: bool,
    pub has_refresh_token: bool,
}

impl McpAuthView {
    fn of(spec: &McpAuthSpec) -> Self {
        let kind = if spec.kind.trim().is_empty() {
            "none".to_string()
        } else {
            spec.kind.clone()
        };
        Self {
            kind,
            header_name: spec.header_name.clone(),
            token_url: spec.token_url.clone(),
            grant_type: spec.grant_type.clone(),
            client_id: spec.client_id.clone(),
            scope: spec.scope.clone(),
            has_token: !spec.token.is_empty(),
            has_header_value: !spec.header_value.is_empty(),
            has_client_secret: !spec.client_secret.is_empty(),
            has_refresh_token: !spec.refresh_token.is_empty(),
        }
    }
}

/// Build a redacted view of `def`, reading its live connection status from the
/// manager and attaching an optional `connect_error` from a just-run (re)connect.
fn view_of(
    def: &McpServerDef,
    manager: &McpManager,
    connect_error: Option<String>,
) -> McpServerView {
    McpServerView {
        name: def.name.clone(),
        transport: def.transport.clone(),
        command: def.command.clone(),
        args: def.args.clone(),
        env_keys: def.env.keys().cloned().collect(),
        url: def.url.clone(),
        auth: McpAuthView::of(&def.auth),
        enabled: def.enabled,
        tools: def.tools.clone(),
        connected: manager.is_connected(&def.name),
        connect_error,
    }
}

/// Whether a transport string is HTTP-flavoured (else stdio). Mirrors
/// [`McpServerDef::is_http`] over a raw string.
fn is_http_transport(transport: &str) -> bool {
    matches!(transport, "http" | "https" | "sse" | "streamable-http")
}

/// Turn a request body into a [`NewMcpServerDef`] under `name`, normalising the
/// transport and validating that it has what it needs (a `command` for stdio, a
/// `url` for http).
fn build_new_def(name: String, body: McpServerBody) -> ApiResult<NewMcpServerDef> {
    let transport = {
        let t = body.transport.trim();
        if t.is_empty() {
            "stdio".to_string()
        } else {
            t.to_ascii_lowercase()
        }
    };
    let http = is_http_transport(&transport);
    if http && body.url.trim().is_empty() {
        return Err(ApiError::bad_request(
            "`url` is required for an http MCP server",
        ));
    }
    if !http && body.command.trim().is_empty() {
        return Err(ApiError::bad_request(
            "`command` is required for a stdio MCP server",
        ));
    }
    Ok(NewMcpServerDef {
        name,
        transport,
        command: body.command,
        args: body.args,
        env: body.env,
        url: body.url,
        auth: body.auth.into_spec(),
        enabled: body.enabled,
        tools: body.tools,
    })
}

/// Connect (or disconnect) `def` through the manager, returning a connect-error
/// string if an enabled server failed to reach — the row is already persisted, so
/// a failure is reported, not raised (it retries on the next boot).
async fn reconcile(manager: &McpManager, def: &McpServerDef) -> Option<String> {
    if !def.enabled {
        manager.disconnect(&def.name);
        return None;
    }
    match manager.connect(def).await {
        Ok(_) => None,
        Err(e) => Some(e.to_string()),
    }
}

async fn list(State(state): State<AppState>, auth: Auth) -> ApiResult<Json<Vec<McpServerView>>> {
    auth.require(Action::Read, "mcp")?;
    let ws = auth.principal().workspace_id;
    let manager = state.mcp_manager();
    let servers = state.store().mcp_servers().list_by_workspace(ws).await?;
    let views = servers.iter().map(|s| view_of(s, &manager, None)).collect();
    Ok(Json(views))
}

async fn create(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<McpServerBody>,
) -> ApiResult<(StatusCode, Json<McpServerView>)> {
    auth.require(Action::Write, "mcp")?;
    auth.require_workspace_admin()?;
    let ws = auth.principal().workspace_id;
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return Err(ApiError::bad_request("server name must not be empty"));
    }
    let new = build_new_def(name, body)?;
    let def = state.store().mcp_servers().create(ws, &new).await?;
    let manager = state.mcp_manager();
    let connect_error = reconcile(&manager, &def).await;
    Ok((
        StatusCode::CREATED,
        Json(view_of(&def, &manager, connect_error)),
    ))
}

async fn update(
    State(state): State<AppState>,
    auth: Auth,
    Path(name): Path<String>,
    Json(mut body): Json<McpServerBody>,
) -> ApiResult<Json<McpServerView>> {
    auth.require(Action::Write, "mcp")?;
    auth.require_workspace_admin()?;
    let ws = auth.principal().workspace_id;
    // The path name is the identity — never rename via update.
    body.name = name.clone();
    // Back-fill omitted secrets / env values from the stored definition, so
    // editing a non-secret field never forces re-entering credentials.
    let prev = state
        .store()
        .mcp_servers()
        .get_by_name(ws, &name)
        .await?
        .ok_or(ApiError::NotFound)?;
    preserve_secrets(&mut body, &prev);
    let new = build_new_def(name, body)?;
    let def = state.store().mcp_servers().upsert_by_name(ws, &new).await?;
    let manager = state.mcp_manager();
    let connect_error = reconcile(&manager, &def).await;
    Ok(Json(view_of(&def, &manager, connect_error)))
}

/// Keep the stored secret / env value whenever the incoming one is blank. Auth
/// secrets are only preserved when the auth kind is unchanged (another kind's
/// stored secret is meaningless to this one); env values are preserved per key.
fn preserve_secrets(body: &mut McpServerBody, prev: &McpServerDef) {
    let same_kind = body
        .auth
        .kind
        .trim()
        .eq_ignore_ascii_case(prev.auth.kind.trim());
    if same_kind {
        if body.auth.token.is_empty() {
            body.auth.token = prev.auth.token.clone();
        }
        if body.auth.header_value.is_empty() {
            body.auth.header_value = prev.auth.header_value.clone();
        }
        if body.auth.client_secret.is_empty() {
            body.auth.client_secret = prev.auth.client_secret.clone();
        }
        if body.auth.refresh_token.is_empty() {
            body.auth.refresh_token = prev.auth.refresh_token.clone();
        }
    }
    for (key, value) in &mut body.env {
        if value.is_empty() {
            if let Some(stored) = prev.env.get(key) {
                *value = stored.clone();
            }
        }
    }
}

async fn delete(
    State(state): State<AppState>,
    auth: Auth,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    auth.require(Action::Write, "mcp")?;
    auth.require_workspace_admin()?;
    let ws = auth.principal().workspace_id;
    // Tear down live tools first, then delete the row (NotFound if absent).
    state.mcp_manager().disconnect(&name);
    state
        .store()
        .mcp_servers()
        .delete_by_name(ws, &name)
        .await
        .map_err(|_| ApiError::NotFound)?;
    Ok(StatusCode::NO_CONTENT)
}
