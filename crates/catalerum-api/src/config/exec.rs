//! Command execution backends and MCP server exposure.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::Secret;

/// Command/code execution (SOUL §20). **Protected, opt-in.** When `enabled`
/// (default off) the binary builds a local `Executor` and registers the
/// `run_command` tool — which still requires the `exec:run` capability (no base
/// role grants it, §19). `allow` restricts which program names may run (empty →
/// any program; still capability-gated). The Local backend is the highest blast
/// radius; container/bao backends land later.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ExecConfig {
    /// Whether to enable the executor + `run_command` / terminal tools.
    pub enabled: bool,
    /// Allowed program names (basename or exact argv[0]); empty = any.
    pub allow: Vec<String>,
    /// Default terminal/exec backend when a workdir doesn't pin one:
    /// `local` | `sandbox` | `container` | `kubernetes`. Empty → `local`.
    pub backend: String,
    /// Shell to launch for interactive terminals (e.g. `/bin/bash`); may carry
    /// arguments (whitespace-separated), e.g. `/usr/bin/env bash`. Empty → a
    /// deterministic, wizard-free default: `/usr/bin/env bash` on Unix and
    /// PowerShell on Windows (`pwsh` when on `PATH`, else `powershell`) for the
    /// local/sandbox host backend, `/bin/sh` inside a container/k8s session. The
    /// user's interactive `$SHELL` is deliberately **not** used — its first-run
    /// setup (zsh/fish new-user wizard) can block the PTY and wedge the session.
    pub shell: String,
    /// Root directory for ephemeral terminal temp dirs (local/sandbox backends).
    /// Empty → the system temp dir.
    pub ephemeral_root: String,
    /// Podman/Docker container backend settings (SOUL §20).
    pub podman: PodmanExecConfig,
    /// Kubernetes backend settings (SOUL §20).
    pub k8s: K8sExecConfig,
    /// Run **one long-lived sandbox per workspace** (the operator posture, SOUL
    /// §20) for the container/kubernetes backends: terminal sessions and
    /// `run_command` `exec` into a single `catalerum-ws-<id>` container/Pod with a
    /// shared persistent `/work` volume, instead of a fresh container per call.
    /// No effect on the local/sandbox backends.
    pub per_workspace: bool,
    /// Destroy an idle per-workspace container after this many seconds with no
    /// live sessions (podman backend; the k8s operator GCs its own). `0` → the
    /// built-in default (1800s). Only meaningful with `per_workspace`.
    pub idle_timeout_secs: u64,
    /// Persistent `/work` volume size for the per-workspace sandbox (k8s PVC size,
    /// e.g. `10Gi`); empty → `10Gi`. Ignored by podman (named volumes are unsized).
    pub volume_size: String,
    /// Network policy for the per-workspace sandbox; empty → **full** network
    /// (the per-workspace default, distinct from the per-session container
    /// backend's `none`). Set e.g. `none` to isolate it.
    pub sandbox_network: String,
}

impl ExecConfig {
    /// The parsed default [`ExecutorKind`](catalerum_core::model::ExecutorKind)
    /// backend (`Local` when unset/unknown).
    #[must_use]
    pub fn backend_kind(&self) -> catalerum_core::model::ExecutorKind {
        catalerum_core::model::ExecutorKind::parse_token(&self.backend)
            .unwrap_or(catalerum_core::model::ExecutorKind::Local)
    }

    /// Idle timeout for the per-workspace sandbox (`0` → 1800s default).
    #[must_use]
    pub fn sandbox_idle_timeout_secs(&self) -> u64 {
        if self.idle_timeout_secs == 0 {
            1800
        } else {
            self.idle_timeout_secs
        }
    }

    /// Persistent `/work` volume size for the per-workspace sandbox (empty →
    /// `10Gi`).
    #[must_use]
    pub fn sandbox_volume_size(&self) -> String {
        let v = self.volume_size.trim();
        if v.is_empty() {
            "10Gi".to_string()
        } else {
            v.to_string()
        }
    }

    /// The default [`SandboxSpec`](catalerum_exec::SandboxSpec) for the
    /// per-workspace sandbox, derived from this config: backend-appropriate
    /// image, full network unless `sandbox_network` narrows it, and the `/work`
    /// volume size. Shared by the binary (sandbox construction) and `AppState`
    /// (the manager's default spec).
    #[must_use]
    pub fn sandbox_spec(&self) -> catalerum_exec::SandboxSpec {
        use catalerum_core::model::ExecutorKind;
        let net = self.sandbox_network.trim();
        let image = match self.backend_kind() {
            ExecutorKind::Kubernetes => self.k8s.image.clone(),
            _ => self.podman.image.clone(),
        };
        catalerum_exec::SandboxSpec {
            image: (!image.trim().is_empty()).then_some(image),
            limits: catalerum_core::provider::ResourceLimits {
                network: (!net.is_empty()).then(|| net.to_string()),
                ..catalerum_core::provider::ResourceLimits::default()
            },
            volume_size: Some(self.sandbox_volume_size()),
        }
    }
}

/// Podman/Docker container terminal backend (SOUL §20): a long-lived container
/// per session, the workdir bind-mounted / a named volume, CPU/mem/net limits.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct PodmanExecConfig {
    /// CLI binary to drive: `podman` (default) or `docker`.
    pub binary: String,
    /// Default container image for a terminal session.
    pub image: String,
    /// Network policy passed to the runtime (`none` default = no egress).
    pub network: String,
}

impl Default for PodmanExecConfig {
    fn default() -> Self {
        Self {
            binary: "podman".to_string(),
            image: "docker.io/library/debian:stable-slim".to_string(),
            network: "none".to_string(),
        }
    }
}

impl PodmanExecConfig {
    /// The CLI binary to invoke (`podman` when unset).
    #[must_use]
    pub fn binary_name(&self) -> &str {
        let b = self.binary.trim();
        if b.is_empty() {
            "podman"
        } else {
            b
        }
    }
}

/// Kubernetes terminal backend (SOUL §20): a long-lived Pod per session,
/// `exec`/`attach` for the PTY, a PVC-mounted workdir. Not a full CRD operator.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct K8sExecConfig {
    /// Namespace to create session Pods in (default `default`).
    pub namespace: String,
    /// Default container image for a session Pod.
    pub image: String,
    /// StorageClass for the per-workdir PVC (empty → the cluster default).
    pub storage_class: String,
}

impl Default for K8sExecConfig {
    fn default() -> Self {
        Self {
            namespace: "default".to_string(),
            image: "docker.io/library/debian:stable-slim".to_string(),
            storage_class: String::new(),
        }
    }
}

/// External MCP servers catalerum connects to **as a client** (SOUL §26 — the
/// inbound half of principle 15). Each entry spawns a stdio MCP server (e.g.
/// `npx @playwright/mcp`) at startup; its tools join the §7 registry as
/// `{server}_{tool}`, each gated on `mcp:use@{server}` — a protected scope no
/// base role holds (§19), so remote tools are reachable only by an owner or an
/// explicitly granted agent, exactly like `run_command`. A server that fails to
/// start is logged and skipped, never blocking boot.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct McpConfig {
    /// The external servers to connect to (each `[[mcp.servers]]`).
    pub servers: Vec<McpServerConfig>,
}

/// One external MCP server to import tools from, over **stdio** (spawn a child
/// process) or **http** (the streamable-HTTP transport, with optional auth).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct McpServerConfig {
    /// Stable name; prefixes the server's tools and scopes the `mcp:use@{name}`
    /// capability. Empty → the entry is skipped.
    pub name: String,
    /// `"stdio"` (default — spawn `command`) or `"http"` (connect to `url`).
    pub transport: String,
    /// Program to spawn (stdio transport, e.g. `npx`, `uvx`, or an absolute path).
    pub command: String,
    /// Arguments passed to `command` (stdio transport).
    pub args: Vec<String>,
    /// Extra environment variables for the child process (stdio transport).
    pub env: BTreeMap<String, String>,
    /// The endpoint URL (http transport, e.g. `https://host/mcp`).
    pub url: String,
    /// How to authenticate (http transport). Default: none.
    pub auth: McpAuthConfig,
    /// Whether to connect this server (default `true`; lets one be disabled
    /// without deleting its config).
    pub enabled: bool,
    /// Optional allow-list of remote tool names to import; empty → import all.
    pub tools: Vec<String>,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            transport: "stdio".to_string(),
            command: String::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            url: String::new(),
            auth: McpAuthConfig::default(),
            // Listing a server is itself opt-in; default it on so `enabled` only
            // ever needs setting to turn one *off*.
            enabled: true,
            tools: Vec::new(),
        }
    }
}

impl McpServerConfig {
    /// Whether this transport is HTTP (`"http"`/`"sse"`/`"streamable-http"`),
    /// else stdio.
    #[must_use]
    pub fn is_http(&self) -> bool {
        matches!(
            self.transport.trim().to_ascii_lowercase().as_str(),
            "http" | "https" | "sse" | "streamable-http"
        )
    }

    /// Whether this entry is usable: named, with a command (stdio) or URL (http).
    #[must_use]
    pub fn is_configured(&self) -> bool {
        if self.name.trim().is_empty() {
            return false;
        }
        if self.is_http() {
            !self.url.trim().is_empty()
        } else {
            !self.command.trim().is_empty()
        }
    }

    /// The env map as ordered `(key, value)` pairs for the spawn API.
    #[must_use]
    pub fn env_pairs(&self) -> Vec<(String, String)> {
        self.env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
}

/// Authentication for an HTTP MCP server (SOUL §26). `kind` selects the mode;
/// only that mode's fields are read. Secrets use [`Secret`] (redacted in `Debug`,
/// supplied via env, never logged).
///
/// - `none` — no credential.
/// - `bearer` — `Authorization: Bearer {token}`.
/// - `header` — a static `{header_name}: {header_value}` (e.g. `X-Api-Key`).
/// - `oauth2` — machine-to-machine SSO: a `client_credentials` service account or
///   a `refresh_token` grant against `token_url`; access tokens are fetched and
///   refreshed automatically.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct McpAuthConfig {
    /// `none` (default) | `bearer` | `header` | `oauth2`.
    pub kind: String,
    /// Bearer token (`kind = "bearer"`).
    pub token: Secret,
    /// Header name (`kind = "header"`).
    pub header_name: String,
    /// Header value (`kind = "header"`).
    pub header_value: Secret,
    /// OAuth2 token endpoint (`kind = "oauth2"`).
    pub token_url: String,
    /// OAuth2 grant: `client_credentials` (default) or `refresh_token`.
    pub grant_type: String,
    /// OAuth2 client id.
    pub client_id: String,
    /// OAuth2 client secret.
    pub client_secret: Secret,
    /// OAuth2 refresh token (`grant_type = "refresh_token"`).
    pub refresh_token: Secret,
    /// OAuth2 scopes, space-separated.
    pub scope: String,
}
