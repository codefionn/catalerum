//! The daemon's on-disk configuration (`config.toml`).
//!
//! The config is the daemon's *floor* of authority (SOUL §19): it names the server
//! to dial, the enrollment token, the directories the machine will serve and at
//! what access, the roots under which the LLM may *request* more access, the
//! command-exec policy, and whether desktop control is enabled. The server can
//! never widen any of this.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use catalerum_core::computer::{DirGrant, DirMode, ExecPolicy};
use serde::{Deserialize, Serialize};

/// A directory the daemon serves, as written in the config.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DirConfig {
    pub path: String,
    #[serde(default)]
    pub mode: DirMode,
}

impl DirConfig {
    /// As the wire [`DirGrant`].
    pub fn as_grant(&self) -> DirGrant {
        DirGrant {
            path: self.path.clone(),
            mode: self.mode,
        }
    }
}

/// The daemon configuration.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    /// The catalerum **API** origin (http/https or ws/wss; trailing slash ok), e.g.
    /// `https://api.catalerum.example.com`. NOT the web UI's address — that serves
    /// the SPA on every path and answers the WS upgrade with a plain `200`.
    pub server_url: String,
    /// The enrollment token minted by `POST /computer-agents` (shown once).
    pub token: String,
    /// Display name for this machine (informational; the server keyed the token).
    #[serde(default)]
    pub name: String,
    /// Directories to serve, each read or read-write. TOML: `[[dir]]` tables.
    #[serde(default, rename = "dir")]
    pub dirs: Vec<DirConfig>,
    /// Roots under which the LLM may request additional runtime access (each
    /// request still needs a human's approval, server-side). Empty ⇒ refuse.
    #[serde(default)]
    pub grantable_roots: Vec<String>,
    /// How command execution is gated (mirrors the server's classifier posture).
    #[serde(default)]
    pub exec_policy: ExecPolicy,
    /// Enable desktop-control ops (screenshot / open-url / notify).
    #[serde(default)]
    pub desktop: bool,
    /// Wrap executed commands in the OS sandbox where available (Landlock on Linux,
    /// `sandbox-exec` on macOS). Default on.
    #[serde(default = "default_true")]
    pub sandbox: bool,
}

fn default_true() -> bool {
    true
}

impl Config {
    /// Load and parse the config file at `path`.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let config: Config =
            toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        anyhow::ensure!(!self.server_url.trim().is_empty(), "server_url is required");
        anyhow::ensure!(!self.token.trim().is_empty(), "token is required");
        Ok(())
    }

    /// The served directories as wire grants.
    pub fn dir_grants(&self) -> Vec<DirGrant> {
        self.dirs.iter().map(DirConfig::as_grant).collect()
    }

    /// Serialize to TOML (used by `enroll` to write the file).
    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).context("serializing config")
    }

    /// Build the `{scheme}://{host}/computer-agents/connect?token=…` URL, mapping
    /// http→ws and https→wss.
    pub fn connect_url(&self) -> String {
        let base = self.server_url.trim().trim_end_matches('/');
        let ws_base = if let Some(rest) = base.strip_prefix("https://") {
            format!("wss://{rest}")
        } else if let Some(rest) = base.strip_prefix("http://") {
            format!("ws://{rest}")
        } else {
            // Already ws/wss (or bare host — assume wss).
            if base.starts_with("ws://") || base.starts_with("wss://") {
                base.to_string()
            } else {
                format!("wss://{base}")
            }
        };
        let token = urlencode(self.token.trim());
        format!("{ws_base}/computer-agents/connect?token={token}")
    }
}

/// Minimal percent-encoding for the token query value (URL-safe base64 tokens only
/// contain `A–Z a–z 0–9 - _`, so this is a safety net for anything unexpected).
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Resolve the default config path: `$CATALERUM_AGENT_CONFIG`, else the per-user
/// config dir (`$XDG_CONFIG_HOME` / `~/.config` on unix, `%APPDATA%` on Windows) +
/// `catalerum-agent/config.toml`.
pub fn default_config_path() -> PathBuf {
    if let Ok(explicit) = std::env::var("CATALERUM_AGENT_CONFIG") {
        if !explicit.trim().is_empty() {
            return PathBuf::from(explicit);
        }
    }
    let base = config_base_dir();
    base.join("catalerum-agent").join("config.toml")
}

#[cfg(not(target_os = "windows"))]
pub fn config_base_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.trim().is_empty() {
            return PathBuf::from(xdg);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".config")
}

#[cfg(target_os = "windows")]
pub fn config_base_dir() -> PathBuf {
    if let Ok(appdata) = std::env::var("APPDATA") {
        if !appdata.trim().is_empty() {
            return PathBuf::from(appdata);
        }
    }
    PathBuf::from(".")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_url_maps_scheme() {
        let mut c = Config {
            server_url: "https://x.example.com/".into(),
            token: "abc-123_XYZ".into(),
            name: "m".into(),
            dirs: vec![],
            grantable_roots: vec![],
            exec_policy: ExecPolicy::Auto,
            desktop: false,
            sandbox: true,
        };
        assert_eq!(
            c.connect_url(),
            "wss://x.example.com/computer-agents/connect?token=abc-123_XYZ"
        );
        c.server_url = "http://localhost:8787".into();
        assert_eq!(
            c.connect_url(),
            "ws://localhost:8787/computer-agents/connect?token=abc-123_XYZ"
        );
    }

    #[test]
    fn toml_roundtrip() {
        let toml = r#"
server_url = "https://x"
token = "t"
name = "srv"
grantable_roots = ["/home/me"]
exec_policy = "always_ask"
desktop = true

[[dir]]
path = "/work"
mode = "read_write"

[[dir]]
path = "/var/log"
mode = "read"
"#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.dirs.len(), 2);
        assert!(c.dirs[0].mode.can_write());
        assert!(!c.dirs[1].mode.can_write());
        assert_eq!(c.exec_policy, ExecPolicy::AlwaysAsk);
        assert!(c.desktop);
        assert!(c.sandbox, "sandbox defaults on");
    }
}
