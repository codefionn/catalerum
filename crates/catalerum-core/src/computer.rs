//! Wire protocol between a catalerum server and an installed **computer agent**
//! daemon (SOUL §19/§20).
//!
//! A *computer agent* is a small program a user installs on a server or desktop
//! (linux / macos / windows). It dials **out** to the catalerum API over an
//! authenticated WebSocket (bearer = its long-lived enrollment token, stored
//! server-side only as a hash), announces the machine's capabilities, and then
//! serves scoped operations the LLM issues through the `computer_*` tools:
//! reading/writing files under configured directories, searching, running
//! commands (gated by an `auto` classifier or explicit approval), requesting
//! access to further directories, and — where enabled — desktop control.
//!
//! The protocol is deliberately small and transport-agnostic: the server sends
//! [`ServerToAgent`] frames, the agent replies with [`AgentToServer`] frames,
//! each request/response correlated by an opaque `id`. Operation-specific result
//! payloads ride in [`OpResponse::data`] as loose JSON (both ends live in this
//! repo and agree on the shape per op), keeping the enum stable as ops evolve.
//!
//! **Authority is two-party.** The agent only ever serves what its *local* config
//! allows (its `dirs`, its `exec_policy`) — a compromised or over-eager server
//! cannot widen that. Server-side capability scoping (`computer:*`, SOUL §19) is
//! the other half: the LLM needs the capability to call the tools at all.

use serde::{Deserialize, Serialize};

/// Bumped when the frame set changes in a non-backward-compatible way. The agent
/// reports the version it speaks in its [`ComputerCapabilities`]; the server logs
/// a mismatch but still attempts best-effort interop.
pub const PROTOCOL_VERSION: u32 = 1;

/// Default wall-clock limit for an [`ComputerOp::Exec`] command when the caller
/// doesn't pass `timeout_secs`. Both ends key off it: the agent kills the command
/// at this deadline, and the server waits a margin longer so the `timed_out`
/// result still arrives as data instead of a dead-air dispatch timeout.
pub const DEFAULT_EXEC_TIMEOUT_SECS: u64 = 300;

/// Hard ceiling on a single [`ComputerOp::Exec`] command's `timeout_secs`.
pub const MAX_EXEC_TIMEOUT_SECS: u64 = 3600;

/// Default wall-clock search budget when the caller omits `timeout_secs`.
pub const DEFAULT_SEARCH_TIMEOUT_SECS: u64 = 10;

/// Hard ceiling on a single [`ComputerOp::Search`]'s `timeout_secs`.
pub const MAX_SEARCH_TIMEOUT_SECS: u64 = 3600;

/// The operating-system family the agent runs on. Drives which sandbox and
/// desktop mechanisms are available (Landlock on Linux, `sandbox-exec` on macOS).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComputerPlatform {
    Linux,
    Macos,
    Windows,
    /// Reported platform not recognised.
    #[default]
    Other,
}

impl ComputerPlatform {
    /// A short human label for UIs.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            ComputerPlatform::Linux => "Linux",
            ComputerPlatform::Macos => "macOS",
            ComputerPlatform::Windows => "Windows",
            ComputerPlatform::Other => "Other",
        }
    }
}

/// Whether a directory grant is read-only or read-write.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirMode {
    /// List + read files.
    #[default]
    Read,
    /// List + read + write/create files.
    ReadWrite,
}

impl DirMode {
    /// Does this grant permit writes?
    #[must_use]
    pub fn can_write(self) -> bool {
        matches!(self, DirMode::ReadWrite)
    }
}

/// A directory the agent exposes, and at what access level. Paths are absolute on
/// the agent's machine; all file ops are confined to (canonicalised) subpaths of
/// one of these (or of a live runtime grant, see [`ComputerOp::GrantAccess`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirGrant {
    pub path: String,
    #[serde(default)]
    pub mode: DirMode,
}

/// How the agent gates command execution locally. The server also applies its own
/// classifier, but the agent's policy is the floor — it can only *narrow*.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecPolicy {
    /// A server-side classifier judges each command safe/unsafe; unsafe commands
    /// escalate to a human approval (SOUL §19). The default posture.
    #[default]
    Auto,
    /// Every command requires explicit human approval before it runs.
    AlwaysAsk,
    /// Trusted machine: commands run without a gate (still audited).
    AlwaysAllow,
    /// Command execution is disabled entirely on this machine.
    Deny,
}

/// The OS-level sandbox wrapped around executed commands, if any. Advertised so
/// the server/UI can show the effective isolation; enforced by the agent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxKind {
    /// No OS sandbox (host-native execution). Highest blast radius.
    #[default]
    None,
    /// Linux Landlock LSM — filesystem access is restricted to the granted dirs.
    Landlock,
    /// macOS `sandbox-exec` profile restricting filesystem access.
    SandboxExec,
}

impl SandboxKind {
    /// A short human label for UIs.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            SandboxKind::None => "none",
            SandboxKind::Landlock => "landlock",
            SandboxKind::SandboxExec => "sandbox-exec",
        }
    }
}

/// The machine capabilities an agent announces on connect (its [`AgentToServer::Hello`]).
/// Persisted by the server (JSONB) so the enrolled-agent list can show the machine
/// even while it is offline, and refreshed on every reconnect.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComputerCapabilities {
    pub platform: ComputerPlatform,
    /// Machine hostname (informational).
    #[serde(default)]
    pub hostname: String,
    /// OS description, e.g. `"Ubuntu 24.04"` (informational).
    #[serde(default)]
    pub os: String,
    /// CPU architecture, e.g. `"x86_64"` / `"aarch64"` (informational).
    #[serde(default)]
    pub arch: String,
    /// Version string of the installed daemon.
    #[serde(default)]
    pub agent_version: String,
    /// Directories the agent serves and at what access level.
    #[serde(default)]
    pub dirs: Vec<DirGrant>,
    /// Roots under which the LLM may *request* additional runtime access (each
    /// request still needs human approval). Empty ⇒ runtime access is refused.
    #[serde(default)]
    pub grantable_roots: Vec<String>,
    /// How command execution is gated on this machine.
    #[serde(default)]
    pub exec_policy: ExecPolicy,
    /// Whether desktop-control ops (screenshot / open-url / notify) are enabled.
    #[serde(default)]
    pub desktop: bool,
    /// The OS sandbox wrapped around executed commands.
    #[serde(default)]
    pub sandbox: SandboxKind,
    /// Protocol version the agent speaks (see [`PROTOCOL_VERSION`]).
    #[serde(default)]
    pub protocol: u32,
}

/// A frame the **server** sends to the agent.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerToAgent {
    /// Liveness probe; the agent replies [`AgentToServer::Pong`].
    Ping,
    /// Perform `op`; the agent replies with an [`OpResponse`] carrying the same `id`.
    Request { id: String, op: ComputerOp },
}

/// A frame the **agent** sends to the server.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentToServer {
    /// First frame after connecting: announces the machine's capabilities.
    Hello { capabilities: ComputerCapabilities },
    /// Reply to a [`ServerToAgent::Ping`].
    Pong,
    /// Reply to a [`ServerToAgent::Request`].
    Response(OpResponse),
}

/// The agent's reply to one [`ServerToAgent::Request`]. Op-specific success data
/// rides in `data` (loose JSON, shape agreed per op); a failure sets `ok = false`
/// and a human-readable `error`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OpResponse {
    /// Correlation id echoed from the request.
    pub id: String,
    pub ok: bool,
    #[serde(default)]
    pub data: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl OpResponse {
    /// A successful response carrying `data`.
    #[must_use]
    pub fn ok(id: impl Into<String>, data: serde_json::Value) -> Self {
        Self {
            id: id.into(),
            ok: true,
            data,
            error: None,
        }
    }

    /// A failed response carrying an error message.
    #[must_use]
    pub fn err(id: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ok: false,
            data: serde_json::Value::Null,
            error: Some(error.into()),
        }
    }
}

/// How a [`ComputerOp::WriteFile`] applies its content.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteMode {
    /// Replace the file's contents (creating it if absent).
    #[default]
    Overwrite,
    /// Create a new file; fail if it already exists.
    CreateNew,
    /// Append to the file (creating it if absent).
    Append,
}

/// A desktop-control action (only honoured when the agent advertises `desktop`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum DesktopAction {
    /// Capture the primary screen; result `data` = `{ "image_base64", "mime" }`.
    Screenshot,
    /// Open a URL in the machine's default browser.
    OpenUrl { url: String },
    /// Show a desktop notification.
    Notify { title: String, body: String },
}

/// An operation the server asks the agent to perform. Each variant's success
/// `data` shape (in [`OpResponse::data`]) is documented on the variant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ComputerOp {
    /// List a directory. `data` = `{ "entries": [{ "name", "path", "kind":
    /// "file"|"dir"|"symlink", "size" }] }`.
    ListDir {
        /// Optional absolute working directory for a relative `path`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        path: String,
    },
    /// Read a UTF-8 text file (optionally a byte window), or preserve a complete
    /// recognized media file when `media_content_type` is set by a
    /// model-capability-aware server. Text data = `{ "content", "truncated",
    /// "size" }`; media data = `{ "content_base64", "content_type", "size" }`.
    ReadFile {
        /// Optional absolute working directory for a relative `path`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        offset: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u64>,
        /// Server-selected MIME type for native image ingestion.
        /// Hidden from the model; `None` keeps the default text-only behavior.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        media_content_type: Option<String>,
    },
    /// Write a text file. `data` = `{ "path", "bytes_written" }`. Needs a
    /// read-write grant covering `path`.
    WriteFile {
        /// Optional absolute working directory for a relative `path`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        path: String,
        content: String,
        #[serde(default)]
        mode: WriteMode,
    },
    /// Recursively broad-search for a string / regex: matches file/directory
    /// *names* as well as file *contents* (plain queries match case-insensitively).
    /// `data` = `{ "matches": [{ "path", "kind": "name"|"content", "line"?,
    /// "text"? }], "truncated": bool, "timed_out": bool }` (`line`/`text` only
    /// on content matches). A timed-out search returns the matches accumulated by
    /// its deadline instead of failing the operation.
    Search {
        /// Optional absolute working directory. When `root` is omitted this is the
        /// search root; a relative `root` is resolved beneath it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        /// Root to search under; defaults to `cwd`, then all granted dirs.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        root: Option<String>,
        query: String,
        /// Treat `query` as a regular expression.
        #[serde(default)]
        regex: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_results: Option<u64>,
        /// Also descend into / match hidden (dot-prefixed) files and directories.
        #[serde(default)]
        include_hidden: bool,
        /// Stop searching after this many seconds and return the matches found so
        /// far. Defaults to [`DEFAULT_SEARCH_TIMEOUT_SECS`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_secs: Option<u64>,
    },
    /// Run a shell command. `data` = `{ "stdout", "stderr", "exit_code",
    /// "timed_out": bool }`. Subject to the machine's [`ExecPolicy`] and sandbox.
    Exec {
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_secs: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stdin: Option<String>,
    },
    /// Ask the agent to grant runtime access to `path` (must be under a
    /// `grantable_roots` entry). `data` = `{ "path", "mode" }` on success. The
    /// human-approval gate lives on the *server* side (the LLM must get the user's
    /// go-ahead first); the agent additionally refuses paths outside its roots.
    GrantAccess {
        /// Optional absolute working directory for a relative `path`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        path: String,
        mode: DirMode,
    },
    /// Stat a path. `data` = `{ "path", "exists": bool, "kind", "size",
    /// "modified" }`.
    Stat {
        /// Optional absolute working directory for a relative `path`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        path: String,
    },
    /// Perform a desktop action (only when `desktop` is advertised).
    Desktop { action: DesktopAction },
}

impl ComputerOp {
    /// A short verb for audit logs / UIs.
    #[must_use]
    pub fn verb(&self) -> &'static str {
        match self {
            ComputerOp::ListDir { .. } => "list_dir",
            ComputerOp::ReadFile { .. } => "read_file",
            ComputerOp::WriteFile { .. } => "write_file",
            ComputerOp::Search { .. } => "search",
            ComputerOp::Exec { .. } => "exec",
            ComputerOp::GrantAccess { .. } => "grant_access",
            ComputerOp::Stat { .. } => "stat",
            ComputerOp::Desktop { .. } => "desktop",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip() {
        let f = ServerToAgent::Request {
            id: "abc".into(),
            op: ComputerOp::ReadFile {
                cwd: None,
                path: "/work/notes.md".into(),
                offset: None,
                limit: Some(100),
                media_content_type: None,
            },
        };
        let s = serde_json::to_string(&f).unwrap();
        assert!(s.contains("\"type\":\"request\""));
        assert!(s.contains("\"op\":\"read_file\""));
        let back: ServerToAgent = serde_json::from_str(&s).unwrap();
        match back {
            ServerToAgent::Request { id, op } => {
                assert_eq!(id, "abc");
                assert_eq!(op.verb(), "read_file");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn hello_roundtrip() {
        let caps = ComputerCapabilities {
            platform: ComputerPlatform::Linux,
            hostname: "srv1".into(),
            dirs: vec![DirGrant {
                path: "/work".into(),
                mode: DirMode::ReadWrite,
            }],
            exec_policy: ExecPolicy::Auto,
            sandbox: SandboxKind::Landlock,
            protocol: PROTOCOL_VERSION,
            ..Default::default()
        };
        let hello = AgentToServer::Hello {
            capabilities: caps.clone(),
        };
        let s = serde_json::to_string(&hello).unwrap();
        let back: AgentToServer = serde_json::from_str(&s).unwrap();
        match back {
            AgentToServer::Hello { capabilities } => {
                assert_eq!(capabilities.platform, ComputerPlatform::Linux);
                assert!(capabilities.dirs[0].mode.can_write());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn response_helpers() {
        let ok = OpResponse::ok("1", serde_json::json!({ "size": 3 }));
        assert!(ok.ok && ok.error.is_none());
        let err = OpResponse::err("2", "nope");
        assert!(!err.ok && err.error.as_deref() == Some("nope"));
    }

    #[test]
    fn legacy_search_without_new_optional_fields_still_deserializes() {
        let op: ComputerOp = serde_json::from_value(serde_json::json!({
            "op": "search",
            "query": "needle"
        }))
        .unwrap();

        assert!(matches!(
            op,
            ComputerOp::Search {
                cwd: None,
                timeout_secs: None,
                ..
            }
        ));
    }
}
