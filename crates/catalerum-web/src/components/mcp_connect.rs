//! The settings **MCP clients** section (SOUL §26/§30) — connect external
//! agents (Claude Code, Codex, Cursor, …) to this workspace over MCP.
//!
//! catalerum serves MCP as **streamable HTTP** (JSON-RPC 2.0 over `POST`) on
//! the API origin:
//! - `POST /mcp` — the whole workspace's tool surface (workspace bearer token),
//! - `POST /mcp/e/{name}` — a scripted endpoint's curated surface (bearer),
//! - `POST /mcp/s/{token}` — a scripted endpoint behind a signed share URL
//!   (the token rides in the path; no header needed).
//!
//! The page is a config generator, not a config store: pick an endpoint, pick a
//! product, and copy the ready-substituted command / config file. A bearer
//! token can be pasted in or minted inline (the same self-scoped `POST /tokens`
//! the API-keys tab uses); until one is present the snippets carry a
//! `YOUR_CATALERUM_TOKEN` placeholder. Nothing typed here is persisted.
//!
//! Product syntax (verified against each product's docs, 2026-07):
//! - Claude Code: `claude mcp add --transport http … --header …` / `.mcp.json`
//!   (`type: "http"`, `url`, `headers`).
//! - Claude Desktop: stdio-only — bridged via `npx mcp-remote` (header split
//!   through `env` so Windows arg-parsing keeps the space intact).
//! - Codex CLI: `codex mcp add --url … --bearer-token-env-var …` /
//!   `[mcp_servers.*]` in `~/.codex/config.toml` (headers are config-only).
//! - Cursor `mcp.json` (`url` + `headers`), VS Code `mcp.json` (`servers`,
//!   `type: "http"`), Windsurf `mcp_config.json` (`serverUrl`), Gemini CLI
//!   `settings.json` (`httpUrl`), Zed `settings.json` (`context_servers`).
//! - Open-source agents: Cline `cline_mcp_settings.json` (`type:
//!   "streamableHttp"` — camelCase, else it falls back to legacy SSE), Continue
//!   `config.yaml` (`mcpServers` list, `type: streamable-http`, header nested
//!   under `requestOptions.headers`), Goose `config.yaml` (`extensions` map,
//!   `type: streamable_http`, URL key is `uri` not `url`), opencode
//!   `opencode.json` (`mcp` object, `type: "remote"`, `oauth: false` so a static
//!   bearer isn't hijacked by its 401→OAuth flow).

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api::{CreateToken, McpEndpoint, MintEndpointToken, MintedEndpointToken};
use crate::auth;
use crate::components::widgets::copy_to_clipboard;
use crate::rest;

/// The stand-in shown in snippets until a real token is pasted or minted.
const TOKEN_PLACEHOLDER: &str = "YOUR_CATALERUM_TOKEN";

/// The env-var name the Codex snippets route the bearer token through (Codex
/// reads header values from the environment, never inline in config).
const CODEX_TOKEN_ENV: &str = "CATALERUM_MCP_TOKEN";

/// The MCP-capable products the page can generate config for, in chip order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum McpClient {
    ClaudeCode,
    Codex,
    Cursor,
    VsCode,
    ClaudeDesktop,
    Windsurf,
    Gemini,
    Zed,
    Cline,
    Continue,
    Goose,
    OpenCode,
    Other,
}

impl McpClient {
    /// Chip-row order (open-source agents grouped after the first-party ones).
    fn all() -> [McpClient; 13] {
        [
            McpClient::ClaudeCode,
            McpClient::Codex,
            McpClient::Cursor,
            McpClient::VsCode,
            McpClient::ClaudeDesktop,
            McpClient::Windsurf,
            McpClient::Gemini,
            McpClient::Zed,
            McpClient::Cline,
            McpClient::Continue,
            McpClient::Goose,
            McpClient::OpenCode,
            McpClient::Other,
        ]
    }

    /// The chip label.
    fn label(self) -> &'static str {
        match self {
            McpClient::ClaudeCode => "Claude Code",
            McpClient::Codex => "Codex CLI",
            McpClient::Cursor => "Cursor",
            McpClient::VsCode => "VS Code",
            McpClient::ClaudeDesktop => "Claude Desktop",
            McpClient::Windsurf => "Windsurf",
            McpClient::Gemini => "Gemini CLI",
            McpClient::Zed => "Zed",
            McpClient::Cline => "Cline",
            McpClient::Continue => "Continue",
            McpClient::Goose => "Goose",
            McpClient::OpenCode => "opencode",
            McpClient::Other => "Other",
        }
    }
}

/// One copyable configuration snippet: a "where this goes" title + the code.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Snippet {
    title: String,
    code: String,
}

impl Snippet {
    fn new(title: impl Into<String>, code: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            code: code.into(),
        }
    }
}

/// The MCP server name a client registers catalerum under: `catalerum` for the
/// whole-workspace endpoint, `catalerum-{name}` for a scripted endpoint (so two
/// endpoints of the same deployment don't collide in one client config).
fn server_name(endpoint: &str) -> String {
    if endpoint.is_empty() {
        "catalerum".to_string()
    } else {
        format!("catalerum-{endpoint}")
    }
}

/// The serve URL for a picked endpoint: `{base}/mcp` for the whole workspace,
/// `{base}/mcp/e/{name}` for a scripted endpoint.
fn mcp_url(base: &str, endpoint: &str) -> String {
    let base = base.trim_end_matches('/');
    if endpoint.is_empty() {
        format!("{base}/mcp")
    } else {
        format!("{base}/mcp/e/{endpoint}")
    }
}

/// `{"Authorization": "Bearer {token}"}` — the header object every JSON-file
/// format shares.
fn headers_json(bearer: &str) -> serde_json::Value {
    serde_json::json!({ "Authorization": format!("Bearer {bearer}") })
}

/// Pretty-print a JSON config body (serialization of a `Value` cannot fail).
fn pretty(v: &serde_json::Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default()
}

/// Build the copy-paste snippets for one product. `bearer` is `Some(token)` for
/// the bearer-authenticated endpoints and `None` for a signed share URL (where
/// the credential rides in the path, so every `Authorization` header — and the
/// helpers that only exist to carry one — is omitted).
fn snippets_for(client: McpClient, sname: &str, url: &str, bearer: Option<&str>) -> Vec<Snippet> {
    match client {
        McpClient::ClaudeCode => {
            let mut cmd = format!("claude mcp add --transport http {sname} {url}");
            if let Some(t) = bearer {
                cmd.push_str(&format!(" --header \"Authorization: Bearer {t}\""));
            }
            let mut server = serde_json::json!({ "type": "http", "url": url });
            if let Some(t) = bearer {
                server["headers"] = headers_json(t);
            }
            let file = serde_json::json!({ "mcpServers": { sname: server } });
            vec![
                Snippet::new("Terminal — one command", cmd),
                Snippet::new(
                    ".mcp.json — project scope (add --scope user for all projects)",
                    pretty(&file),
                ),
            ]
        }
        McpClient::Codex => {
            let (cmd, toml) = match bearer {
                Some(t) => (
                    format!(
                        "export {CODEX_TOKEN_ENV}=\"{t}\"\n\
                         codex mcp add {sname} --url {url} --bearer-token-env-var {CODEX_TOKEN_ENV}"
                    ),
                    format!(
                        "[mcp_servers.{sname}]\n\
                         url = \"{url}\"\n\
                         # reads the token from the environment: export {CODEX_TOKEN_ENV}=\"{t}\"\n\
                         bearer_token_env_var = \"{CODEX_TOKEN_ENV}\""
                    ),
                ),
                None => (
                    format!("codex mcp add {sname} --url {url}"),
                    format!("[mcp_servers.{sname}]\nurl = \"{url}\""),
                ),
            };
            vec![
                Snippet::new("Terminal — one command", cmd),
                Snippet::new("~/.codex/config.toml", toml),
            ]
        }
        McpClient::Cursor => {
            let mut server = serde_json::json!({ "url": url });
            if let Some(t) = bearer {
                server["headers"] = headers_json(t);
            }
            let file = serde_json::json!({ "mcpServers": { sname: server } });
            vec![Snippet::new(
                "~/.cursor/mcp.json — global (or .cursor/mcp.json in a project)",
                pretty(&file),
            )]
        }
        McpClient::VsCode => {
            let mut server = serde_json::json!({ "type": "http", "url": url });
            if let Some(t) = bearer {
                server["headers"] = headers_json(t);
            }
            let mut add = serde_json::json!({ "name": sname, "type": "http", "url": url });
            if let Some(t) = bearer {
                add["headers"] = headers_json(t);
            }
            let file = serde_json::json!({ "servers": { sname: server } });
            vec![
                Snippet::new(
                    "Terminal — one command",
                    format!(
                        "code --add-mcp '{}'",
                        serde_json::to_string(&add).unwrap_or_default()
                    ),
                ),
                Snippet::new(".vscode/mcp.json — workspace", pretty(&file)),
            ]
        }
        McpClient::ClaudeDesktop => {
            // Claude Desktop speaks stdio only, so the HTTP endpoint is bridged
            // through `npx mcp-remote`. The header value goes through `env` with
            // no space around the `:` in args — the documented workaround for
            // Windows arg-splitting mangling "Authorization: Bearer …".
            let server = match bearer {
                Some(t) => serde_json::json!({
                    "command": "npx",
                    "args": ["-y", "mcp-remote", url, "--header", "Authorization:${AUTH_HEADER}"],
                    "env": { "AUTH_HEADER": format!("Bearer {t}") },
                }),
                None => serde_json::json!({
                    "command": "npx",
                    "args": ["-y", "mcp-remote", url],
                }),
            };
            let file = serde_json::json!({ "mcpServers": { sname: server } });
            vec![Snippet::new(
                "claude_desktop_config.json — Settings → Developer → Edit Config (needs Node; \
                 the app is stdio-only, so mcp-remote bridges to the HTTP endpoint)",
                pretty(&file),
            )]
        }
        McpClient::Windsurf => {
            let mut server = serde_json::json!({ "serverUrl": url });
            if let Some(t) = bearer {
                server["headers"] = headers_json(t);
            }
            let file = serde_json::json!({ "mcpServers": { sname: server } });
            vec![Snippet::new(
                "~/.codeium/windsurf/mcp_config.json",
                pretty(&file),
            )]
        }
        McpClient::Gemini => {
            let mut cmd = format!("gemini mcp add --transport http {sname} {url}");
            if let Some(t) = bearer {
                cmd.push_str(&format!(" --header \"Authorization: Bearer {t}\""));
            }
            let mut server = serde_json::json!({ "httpUrl": url });
            if let Some(t) = bearer {
                server["headers"] = headers_json(t);
            }
            let file = serde_json::json!({ "mcpServers": { sname: server } });
            vec![
                Snippet::new("Terminal — one command", cmd),
                Snippet::new("~/.gemini/settings.json", pretty(&file)),
            ]
        }
        McpClient::Zed => {
            let mut server = serde_json::json!({ "url": url });
            if let Some(t) = bearer {
                server["headers"] = headers_json(t);
            }
            let file = serde_json::json!({ "context_servers": { sname: server } });
            vec![Snippet::new("~/.config/zed/settings.json", pretty(&file))]
        }
        McpClient::Cline => {
            // Cline lists SSE first in its transport union, so a URL with no
            // `type` silently defaults to legacy SSE (405s here). Always emit
            // "streamableHttp" (camelCase, capital H). The UI's Remote Servers
            // tab can't set custom headers — the bearer goes in via this JSON.
            let mut server = serde_json::json!({
                "type": "streamableHttp",
                "url": url,
                "disabled": false,
                "autoApprove": [],
            });
            if let Some(t) = bearer {
                server["headers"] = headers_json(t);
            }
            let file = serde_json::json!({ "mcpServers": { sname: server } });
            vec![Snippet::new(
                "cline_mcp_settings.json — Cline panel → MCP Servers → Configure MCP Servers",
                pretty(&file),
            )]
        }
        McpClient::Continue => {
            // config.yaml era: `mcpServers` is a LIST, transport is hyphenated
            // `streamable-http`, and the auth header nests two levels down under
            // `requestOptions.headers` (not a flat `headers`).
            let mut yaml = format!(
                "mcpServers:\n  - name: {sname}\n    type: streamable-http\n    url: {url}\n"
            );
            if let Some(t) = bearer {
                yaml.push_str(&format!(
                    "    requestOptions:\n      headers:\n        Authorization: Bearer {t}\n"
                ));
            }
            vec![Snippet::new(
                "~/.continue/config.yaml (or a .continue/mcpServers/*.yaml block)",
                yaml,
            )]
        }
        McpClient::Goose => {
            // Goose calls MCP servers "extensions" (a map keyed by name); the
            // transport is snake_case `streamable_http` and the URL key is
            // `uri`, not `url`. The bearer lives inside the `headers` map.
            let mut yaml = format!(
                "extensions:\n  {sname}:\n    enabled: true\n    type: streamable_http\n    \
                 name: {sname}\n    uri: {url}\n"
            );
            if let Some(t) = bearer {
                yaml.push_str(&format!(
                    "    headers:\n      Authorization: \"Bearer {t}\"\n"
                ));
            }
            yaml.push_str("    timeout: 300\n");
            vec![Snippet::new(
                "~/.config/goose/config.yaml — or run: goose configure → Add Extension → \
                 Remote Extension (Streamable HTTP)",
                yaml,
            )]
        }
        McpClient::OpenCode => {
            // SST's opencode: `mcp` object, transport `"remote"`. It auto-runs
            // an OAuth flow on a 401, so pin `oauth: false` for a static bearer
            // to keep it using the header we supply.
            let mut server = serde_json::json!({
                "type": "remote",
                "url": url,
                "enabled": true,
            });
            if let Some(t) = bearer {
                server["oauth"] = serde_json::Value::Bool(false);
                server["headers"] = headers_json(t);
            }
            let file = serde_json::json!({
                "$schema": "https://opencode.ai/config.json",
                "mcp": { sname: server },
            });
            vec![Snippet::new(
                "opencode.json — project root, or ~/.config/opencode/opencode.json",
                pretty(&file),
            )]
        }
        McpClient::Other => {
            let mut curl = format!(
                "curl -sS -X POST {url} \\\n  -H \"Content-Type: application/json\" \\\n  \
                 -H \"Accept: application/json, text/event-stream\" \\\n"
            );
            if let Some(t) = bearer {
                curl.push_str(&format!("  -H \"Authorization: Bearer {t}\" \\\n"));
            }
            curl.push_str("  -d '{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}'");
            let bridge = match bearer {
                Some(t) => {
                    format!("npx -y mcp-remote {url} --header \"Authorization: Bearer {t}\"")
                }
                None => format!("npx -y mcp-remote {url}"),
            };
            vec![
                Snippet::new(
                    "Any MCP client — it's streamable HTTP (JSON-RPC 2.0 over POST); \
                     try it with curl",
                    curl,
                ),
                Snippet::new("Stdio-only clients — bridge with mcp-remote", bridge),
            ]
        }
    }
}

/// Days until a Unix-seconds expiry, floored at zero — for the share-URL note.
fn days_until(expires_at: i64, now_ms: f64) -> i64 {
    let ms_left = (expires_at as f64) * 1000.0 - now_ms;
    (ms_left / 86_400_000.0).ceil().max(0.0) as i64
}

/// **MCP clients** — pick an endpoint + a product, get copy-paste config. See
/// the module docs for the endpoint/auth model.
#[component]
pub fn McpConnectSection() -> impl IntoView {
    // The API origin the snippets point at — fixed per mount.
    let base = StoredValue::new(crate::api::api_base());

    // The workspace's scripted endpoints (best-effort: without the list the
    // page still serves the whole-workspace endpoint).
    let endpoints = RwSignal::new(Vec::<McpEndpoint>::new());
    // Picked endpoint name; "" = the whole-workspace `/mcp`.
    let selected = RwSignal::new(String::new());
    // Picked product.
    let client = RwSignal::new(McpClient::ClaudeCode);
    // The bearer token woven into the snippets (pasted or minted); empty =
    // placeholder text.
    let token = RwSignal::new(String::new());
    // An active share URL for the selected scripted endpoint (replaces bearer).
    let share = RwSignal::new(Option::<MintedEndpointToken>::None);

    let minting = RwSignal::new(false);
    let sharing = RwSignal::new(false);
    let error = RwSignal::new(Option::<String>::None);
    let notice = RwSignal::new(Option::<String>::None);
    // Index of the snippet whose Copy button just fired (flashes "Copied ✓").
    let copied = RwSignal::new(Option::<usize>::None);

    spawn_local(async move {
        let tok = auth::resolve_token();
        if let Ok(list) = rest::list_mcp_endpoints(tok.as_deref()).await {
            endpoints.set(list);
        }
    });

    // The effective serve URL: a minted share URL wins; otherwise the picked
    // endpoint's bearer-authenticated URL.
    let url = Signal::derive(move || match share.get() {
        Some(s) => format!("{}{}", base.get_value().trim_end_matches('/'), s.path),
        None => mcp_url(&base.get_value(), &selected.get()),
    });

    let snippets = Signal::derive(move || {
        let tok = token.get();
        let tok = tok.trim();
        let bearer = if share.get().is_some() {
            None
        } else if tok.is_empty() {
            Some(TOKEN_PLACEHOLDER.to_string())
        } else {
            Some(tok.to_string())
        };
        snippets_for(
            client.get(),
            &server_name(&selected.get()),
            &url.get(),
            bearer.as_deref(),
        )
    });

    // Mint a 90-day full-role bearer token (the same self-scoped `POST /tokens`
    // as the API-keys tab) and weave it into the snippets.
    let issue_token = move || {
        minting.set(true);
        error.set(None);
        notice.set(None);
        spawn_local(async move {
            let tok = auth::resolve_token();
            let body = CreateToken {
                ttl_days: 90,
                grant: None,
            };
            match rest::create_token(tok.as_deref(), &body).await {
                Ok(created) => {
                    token.set(created.token);
                    share.set(None);
                    notice.set(Some(
                        "Token issued (valid 90 days) and filled into the snippets below. \
                         It is shown only here, only now — copy the config before leaving. \
                         Revoke it any time under API keys."
                            .to_string(),
                    ));
                }
                Err(e) => error.set(Some(e.to_string())),
            }
            minting.set(false);
        });
    };

    // Mint a signed share URL for the selected scripted endpoint (server
    // default TTL): the credential rides in the path, so the snippets drop the
    // Authorization header entirely.
    let mint_share = move || {
        let name = selected.get_untracked();
        let Some(id) = endpoints
            .get_untracked()
            .iter()
            .find(|e| e.name == name)
            .map(|e| e.id.clone())
        else {
            return;
        };
        sharing.set(true);
        error.set(None);
        notice.set(None);
        spawn_local(async move {
            let tok = auth::resolve_token();
            let body = MintEndpointToken { ttl_days: None };
            match rest::mint_mcp_endpoint_token(tok.as_deref(), &id, &body).await {
                Ok(minted) => share.set(Some(minted)),
                Err(e) => error.set(Some(e.to_string())),
            }
            sharing.set(false);
        });
    };

    view! {
        <section class="settings-section">
            <p class="settings-blurb">
                "Use this workspace from other AI products over MCP — Claude Code, Codex, "
                "Cursor, and friends. Pick what to expose and which product to configure; "
                "the commands and config below are ready to copy. A client authenticates "
                "with a bearer token: paste one, or issue one here."
            </p>

            // Endpoint picker — only once a scripted endpoint exists; with none,
            // the sole choice is the whole-workspace endpoint, so no picker.
            <Show when=move || !endpoints.with(Vec::is_empty) fallback=|| ().into_view()>
                <div class="settings-field">
                    <label class="settings-label">"What to expose"</label>
                    <select
                        class="settings-input"
                        prop:value=move || selected.get()
                        on:change=move |ev| {
                            selected.set(event_target_value(&ev));
                            share.set(None);
                            copied.set(None);
                        }
                    >
                        <option value="">"Whole workspace — every tool your token allows"</option>
                        {move || {
                            endpoints
                                .get()
                                .into_iter()
                                .map(|e| {
                                    let label = match (e.description.is_empty(), e.enabled) {
                                        (true, true) => e.name.clone(),
                                        (false, true) => format!("{} — {}", e.name, e.description),
                                        (true, false) => format!("{} (disabled)", e.name),
                                        (false, false) => {
                                            format!("{} — {} (disabled)", e.name, e.description)
                                        }
                                    };
                                    view! { <option value=e.name.clone()>{label}</option> }
                                })
                                .collect::<Vec<_>>()
                        }}
                    </select>
                </div>
            </Show>

            <div class="settings-field">
                <label class="settings-label">"Server URL"</label>
                <div class="mcp-url-row">
                    <span class="mcp-url">{move || url.get()}</span>
                    <button
                        class="settings-btn settings-btn-mini"
                        on:click=move |_| copy_to_clipboard(&url.get_untracked())
                    >
                        "Copy"
                    </button>
                </div>
            </div>

            {move || {
                share
                    .get()
                    .map(|s| {
                        let days = days_until(s.expires_at, js_sys::Date::now());
                        view! {
                            <div class="settings-form-notice">
                                {format!(
                                    "Public share URL active — the credential is in the URL, no \
                                     header needed. Anyone with this link can call the endpoint \
                                     for the next {days} days.",
                                )}
                                <button
                                    class="settings-btn settings-btn-mini mcp-share-back"
                                    on:click=move |_| share.set(None)
                                >
                                    "Use a bearer token instead"
                                </button>
                            </div>
                        }
                        .into_any()
                    })
                    .unwrap_or_else(|| {
                        view! {
                            <div class="settings-field">
                                <label class="settings-label">"Bearer token"</label>
                                <div class="mcp-url-row">
                                    <input
                                        class="settings-input mcp-token-input"
                                        r#type="text"
                                        placeholder="paste an API token, or issue one →"
                                        prop:value=move || token.get()
                                        on:input=move |ev| token.set(event_target_value(&ev))
                                    />
                                    <button
                                        class="settings-btn"
                                        disabled=move || minting.get()
                                        on:click=move |_| issue_token()
                                    >
                                        {move || {
                                            if minting.get() { "Issuing…" } else { "Issue token" }
                                        }}
                                    </button>
                                    <Show
                                        when=move || !selected.with(String::is_empty)
                                        fallback=|| ().into_view()
                                    >
                                        <button
                                            class="settings-btn"
                                            disabled=move || sharing.get()
                                            on:click=move |_| mint_share()
                                        >
                                            {move || {
                                                if sharing.get() {
                                                    "Minting…"
                                                } else {
                                                    "Public share URL"
                                                }
                                            }}
                                        </button>
                                    </Show>
                                </div>
                                <span class="mcp-share-note">
                                    "An issued token carries your full role in this workspace for "
                                    "90 days; to hand out less, scope a token to a capability "
                                    "grant under API keys and paste it here."
                                </span>
                            </div>
                        }
                        .into_any()
                    })
            }}

            <Show when=move || error.with(Option::is_some) fallback=|| ().into_view()>
                <div class="settings-form-error">{move || error.get().unwrap_or_default()}</div>
            </Show>
            <Show when=move || notice.with(Option::is_some) fallback=|| ().into_view()>
                <div class="settings-form-notice">{move || notice.get().unwrap_or_default()}</div>
            </Show>

            <div class="settings-field">
                <label class="settings-label">"Configure"</label>
                <div class="mcp-clients">
                    {McpClient::all()
                        .into_iter()
                        .map(|c| {
                            let active = move || client.get() == c;
                            view! {
                                <button
                                    class="mcp-client-chip"
                                    class:mcp-client-chip-active=active
                                    on:click=move |_| {
                                        client.set(c);
                                        copied.set(None);
                                    }
                                >
                                    {c.label()}
                                </button>
                            }
                        })
                        .collect::<Vec<_>>()}
                </div>
            </div>

            {move || {
                snippets
                    .get()
                    .into_iter()
                    .enumerate()
                    .map(|(i, s)| {
                        let code = s.code.clone();
                        view! {
                            <div class="mcp-snippet">
                                <div class="mcp-snippet-head">
                                    <span class="settings-label">{s.title.clone()}</span>
                                    <button
                                        class="settings-btn settings-btn-mini"
                                        on:click=move |_| {
                                            copy_to_clipboard(&code);
                                            copied.set(Some(i));
                                        }
                                    >
                                        {move || {
                                            if copied.get() == Some(i) { "Copied ✓" } else { "Copy" }
                                        }}
                                    </button>
                                </div>
                                <pre class="mcp-snippet-code">{s.code.clone()}</pre>
                            </div>
                        }
                    })
                    .collect::<Vec<_>>()
            }}

            <p class="settings-blurb mcp-footnote">
                "The URL lives on the API origin (not this page's address). Tokens can be "
                "listed and revoked under " <strong>"API keys"</strong>
                "; a public share URL expires on its own and can be re-minted here."
            </p>
        </section>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_name_and_url_cover_both_endpoint_kinds() {
        assert_eq!(server_name(""), "catalerum");
        assert_eq!(server_name("wiki"), "catalerum-wiki");
        assert_eq!(
            mcp_url("https://api.example.com/", ""),
            "https://api.example.com/mcp"
        );
        assert_eq!(
            mcp_url("https://api.example.com", "wiki"),
            "https://api.example.com/mcp/e/wiki"
        );
    }

    /// Every JSON-file snippet must parse back as JSON — the copy-paste must
    /// never hand the user a syntax error.
    #[test]
    fn json_snippets_parse_for_every_client() {
        for client in McpClient::all() {
            for bearer in [Some("sk-test"), None] {
                for s in snippets_for(client, "catalerum", "https://api.x.test/mcp", bearer) {
                    let code = s.code.trim();
                    if code.starts_with('{') {
                        serde_json::from_str::<serde_json::Value>(code).unwrap_or_else(|e| {
                            panic!("{:?} snippet {:?} is not valid JSON: {e}", client, s.title)
                        });
                    }
                }
            }
        }
    }

    /// With a bearer token, every product's snippets must carry it somewhere;
    /// with a share URL (no bearer), no snippet may emit an Authorization
    /// header or a Bearer credential.
    #[test]
    fn bearer_is_woven_in_and_share_mode_drops_it() {
        for client in McpClient::all() {
            let with = snippets_for(client, "catalerum", "https://api.x.test/mcp", Some("sk-42"));
            assert!(
                with.iter().any(|s| s.code.contains("sk-42")),
                "{client:?} snippets never mention the token"
            );
            let without = snippets_for(
                client,
                "catalerum-wiki",
                "https://api.x.test/mcp/s/signed",
                None,
            );
            for s in &without {
                assert!(
                    !s.code.contains("Authorization") && !s.code.contains("Bearer"),
                    "{client:?} share-mode snippet {:?} still carries auth",
                    s.title
                );
                assert!(s.code.contains("/mcp/s/signed"));
            }
        }
    }

    #[test]
    fn claude_code_uses_http_transport_and_mcp_json_shape() {
        let snips = snippets_for(
            McpClient::ClaudeCode,
            "catalerum",
            "https://api.x.test/mcp",
            Some("tok"),
        );
        assert!(snips[0]
            .code
            .starts_with("claude mcp add --transport http catalerum"));
        assert!(snips[0]
            .code
            .contains("--header \"Authorization: Bearer tok\""));
        let file: serde_json::Value = serde_json::from_str(&snips[1].code).unwrap();
        assert_eq!(file["mcpServers"]["catalerum"]["type"], "http");
        assert_eq!(
            file["mcpServers"]["catalerum"]["headers"]["Authorization"],
            "Bearer tok"
        );
    }

    #[test]
    fn codex_routes_the_token_through_the_environment() {
        let snips = snippets_for(
            McpClient::Codex,
            "catalerum",
            "https://api.x.test/mcp",
            Some("tok"),
        );
        // The CLI line exports the env var, then registers with
        // --bearer-token-env-var — Codex never takes the raw header inline.
        assert!(snips[0].code.contains("export CATALERUM_MCP_TOKEN=\"tok\""));
        assert!(snips[0]
            .code
            .contains("--bearer-token-env-var CATALERUM_MCP_TOKEN"));
        assert!(snips[1].code.contains("[mcp_servers.catalerum]"));
        assert!(snips[1]
            .code
            .contains("bearer_token_env_var = \"CATALERUM_MCP_TOKEN\""));
    }

    /// The product-specific field spellings the docs mandate — a regression
    /// here ships broken config to users.
    #[test]
    fn product_specific_field_names_hold() {
        let url = "https://api.x.test/mcp";
        let get = |c| snippets_for(c, "catalerum", url, Some("t"));
        let vs: serde_json::Value = serde_json::from_str(&get(McpClient::VsCode)[1].code).unwrap();
        assert!(vs["servers"]["catalerum"]["type"] == "http");
        let ws: serde_json::Value =
            serde_json::from_str(&get(McpClient::Windsurf)[0].code).unwrap();
        assert!(ws["mcpServers"]["catalerum"]["serverUrl"] == url);
        let gm: serde_json::Value = serde_json::from_str(&get(McpClient::Gemini)[1].code).unwrap();
        assert!(gm["mcpServers"]["catalerum"]["httpUrl"] == url);
        let zed: serde_json::Value = serde_json::from_str(&get(McpClient::Zed)[0].code).unwrap();
        assert!(zed["context_servers"]["catalerum"]["url"] == url);
        // Claude Desktop is stdio-only: bridged via mcp-remote, header via env.
        let cd: serde_json::Value =
            serde_json::from_str(&get(McpClient::ClaudeDesktop)[0].code).unwrap();
        assert_eq!(cd["mcpServers"]["catalerum"]["command"], "npx");
        assert_eq!(
            cd["mcpServers"]["catalerum"]["env"]["AUTH_HEADER"],
            "Bearer t"
        );
    }

    /// The open-source agents each spell the transport/URL/header differently;
    /// a regression here ships broken config. Lock the load-bearing spellings.
    #[test]
    fn oss_agent_field_names_hold() {
        let url = "https://api.x.test/mcp";
        let get = |c| snippets_for(c, "catalerum", url, Some("t"));
        // Cline: camelCase `streamableHttp`, flat `url` + `headers`.
        let cline: serde_json::Value =
            serde_json::from_str(&get(McpClient::Cline)[0].code).unwrap();
        assert_eq!(cline["mcpServers"]["catalerum"]["type"], "streamableHttp");
        assert_eq!(cline["mcpServers"]["catalerum"]["url"], url);
        assert_eq!(
            cline["mcpServers"]["catalerum"]["headers"]["Authorization"],
            "Bearer t"
        );
        // opencode: `mcp` container, `type: "remote"`, `oauth: false` for static
        // bearer, flat `headers`.
        let oc: serde_json::Value =
            serde_json::from_str(&get(McpClient::OpenCode)[0].code).unwrap();
        assert_eq!(oc["mcp"]["catalerum"]["type"], "remote");
        assert_eq!(oc["mcp"]["catalerum"]["oauth"], false);
        assert_eq!(oc["mcp"]["catalerum"]["url"], url);
        assert_eq!(
            oc["mcp"]["catalerum"]["headers"]["Authorization"],
            "Bearer t"
        );
        // Continue (YAML): hyphenated transport, header nested under
        // `requestOptions.headers`.
        let cont = &get(McpClient::Continue)[0].code;
        assert!(cont.contains("type: streamable-http"));
        assert!(cont.contains("requestOptions:"));
        assert!(cont.contains("        Authorization: Bearer t"));
        // Goose (YAML): snake_case transport, URL key is `uri` (never `url`).
        let goose = &get(McpClient::Goose)[0].code;
        assert!(goose.contains("type: streamable_http"));
        assert!(goose.contains("uri: https://api.x.test/mcp"));
        assert!(!goose.contains("url:"));
    }

    #[test]
    fn placeholder_flows_like_a_real_token() {
        let snips = snippets_for(
            McpClient::Cursor,
            "catalerum",
            "https://api.x.test/mcp",
            Some(TOKEN_PLACEHOLDER),
        );
        assert!(snips[0].code.contains("Bearer YOUR_CATALERUM_TOKEN"));
    }

    #[test]
    fn share_expiry_days_floor_at_zero() {
        // 10 days out (now = 0 ms), and already expired.
        assert_eq!(days_until(864_000, 0.0), 10);
        assert_eq!(days_until(0, 864_000_000.0), 0);
    }
}
