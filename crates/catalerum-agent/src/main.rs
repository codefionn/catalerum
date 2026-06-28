//! `catalerum-agent` — the installable **computer agent** daemon (SOUL §19/§20).
//!
//! Runs on a server or desktop (Linux / macOS / Windows). It dials out to a
//! catalerum server over an authenticated WebSocket and serves scoped file /
//! search / exec / desktop operations the LLM drives through the `computer_*`
//! tools. What it will serve is fixed by its local config (directories, exec
//! policy, desktop) — the server can never widen that.
//!
//! Usage:
//! - `catalerum-agent enroll --server <URL> --token <TOKEN> --name <NAME> \
//!    --rw <DIR> [--ro <DIR>] [--grantable-root <DIR>] [--exec-policy auto] \
//!    [--desktop]` — write the config file, then
//! - `catalerum-agent run` (or just `catalerum-agent`) — connect and serve.
//! - `catalerum-agent install-service` (Linux) — install + start it as a systemd
//!   **user** service so it survives reboots (with lingering) and restarts on crash.

mod client;
mod config;
mod ops;
mod sandbox;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use catalerum_core::computer::{DirGrant, DirMode, ExecPolicy};
use clap::{Parser, Subcommand};

use crate::config::{default_config_path, Config, DirConfig};
use crate::ops::AgentState;

#[derive(Parser)]
#[command(
    name = "catalerum-agent",
    version,
    about = "catalerum computer-agent daemon"
)]
struct Cli {
    /// Path to the config file (defaults to the per-user config dir).
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Connect to the server and serve operations (the default).
    Run,
    /// Write a config file from the given options (then run `catalerum-agent`).
    Enroll(EnrollArgs),
    /// Print the effective config (secrets redacted) and exit.
    Show,
    /// Install `catalerum-agent run` as a systemd **user** service and start it.
    #[cfg(target_os = "linux")]
    InstallService,
}

#[derive(clap::Args)]
struct EnrollArgs {
    /// The catalerum **API** origin the daemon dials, e.g.
    /// `https://api.catalerum.example.com` — NOT the web UI's address (standard
    /// deployments serve the API on the `api.`-prefixed host; the web UI's
    /// "Enroll a computer" dialog prints the exact command with the right URL).
    #[arg(long)]
    server: String,
    /// The enrollment token from `POST /computer-agents` (shown once).
    #[arg(long)]
    token: String,
    /// Display name for this machine.
    #[arg(long, default_value = "")]
    name: String,
    /// A directory to serve **read-write** (repeatable).
    #[arg(long = "rw", value_name = "DIR")]
    rw: Vec<String>,
    /// A directory to serve **read-only** (repeatable).
    #[arg(long = "ro", value_name = "DIR")]
    ro: Vec<String>,
    /// A root under which the LLM may request further access (repeatable).
    #[arg(long = "grantable-root", value_name = "DIR")]
    grantable_root: Vec<String>,
    /// Command exec policy: `auto` (default), `always_ask`, `always_allow`, `deny`.
    #[arg(long, default_value = "auto")]
    exec_policy: String,
    /// Enable desktop control (screenshot / open-url / notify).
    #[arg(long)]
    desktop: bool,
    /// Disable the OS command sandbox (Landlock / sandbox-exec). Not recommended.
    #[arg(long)]
    no_sandbox: bool,
    /// Overwrite an existing config file.
    #[arg(long)]
    force: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // rustls has no (or, under workspace feature unification, several) crypto
    // provider features enabled, so it cannot auto-pick a process-level
    // CryptoProvider and panics on the first wss:// connect. Pin ring explicitly
    // before dialing; an Err just means a provider was already installed, which
    // is equally fine.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cli = Cli::parse();
    let config_path = cli.config.clone().unwrap_or_else(default_config_path);

    match cli.command.unwrap_or(Command::Run) {
        Command::Enroll(args) => enroll(&config_path, args),
        Command::Show => show(&config_path),
        Command::Run => run(&config_path).await,
        #[cfg(target_os = "linux")]
        Command::InstallService => install_service(&config_path),
    }
}

/// Write a config file from the enroll options.
fn enroll(path: &std::path::Path, args: EnrollArgs) -> Result<()> {
    if path.exists() && !args.force {
        anyhow::bail!(
            "config already exists at {} (pass --force to overwrite)",
            path.display()
        );
    }
    let exec_policy = parse_exec_policy(&args.exec_policy)?;
    let mut dirs: Vec<DirConfig> = Vec::new();
    for p in args.rw {
        dirs.push(DirConfig {
            path: p,
            mode: DirMode::ReadWrite,
        });
    }
    for p in args.ro {
        dirs.push(DirConfig {
            path: p,
            mode: DirMode::Read,
        });
    }
    let config = Config {
        server_url: args.server,
        token: args.token,
        name: args.name,
        dirs,
        grantable_roots: args.grantable_root,
        exec_policy,
        desktop: args.desktop,
        sandbox: !args.no_sandbox,
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config directory {}", parent.display()))?;
    }
    std::fs::write(path, config.to_toml()?)
        .with_context(|| format!("writing config {}", path.display()))?;
    println!("wrote config to {}", path.display());
    println!("start the agent with: catalerum-agent run");
    #[cfg(target_os = "linux")]
    println!("or install it as a systemd user service: catalerum-agent install-service");
    Ok(())
}

/// Install (and start) the daemon as a systemd **user** service.
///
/// Writes `~/.config/systemd/user/catalerum-agent.service` pointing at the current
/// executable and the resolved config, then `daemon-reload` + `enable` + `restart`
/// (restart, not `--now`, so re-installing over a running service picks up a new
/// binary/config). User services stop at logout unless lingering is on — printed
/// as a hint rather than run, since `loginctl enable-linger` can require root.
#[cfg(target_os = "linux")]
fn install_service(config_path: &std::path::Path) -> Result<()> {
    // Fail early on a missing/broken config — the service would just crash-loop.
    Config::load(config_path).with_context(|| {
        format!(
            "loading config {} — run `catalerum-agent enroll …` first",
            config_path.display()
        )
    })?;
    let exe = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .context("resolving the catalerum-agent executable path")?;
    let config_abs = std::fs::canonicalize(config_path)
        .with_context(|| format!("resolving config path {}", config_path.display()))?;

    // systemd honours $XDG_CONFIG_HOME for user units, same as our config lookup.
    let unit_dir = crate::config::config_base_dir()
        .join("systemd")
        .join("user");
    std::fs::create_dir_all(&unit_dir)
        .with_context(|| format!("creating unit directory {}", unit_dir.display()))?;
    let unit_path = unit_dir.join("catalerum-agent.service");
    let unit = format!(
        "[Unit]\n\
         Description=catalerum computer-agent daemon\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         ExecStart=\"{}\" --config \"{}\" run\n\
         Restart=always\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exe.display(),
        config_abs.display()
    );
    std::fs::write(&unit_path, unit)
        .with_context(|| format!("writing unit {}", unit_path.display()))?;

    systemctl_user(&["daemon-reload"])?;
    systemctl_user(&["enable", "catalerum-agent.service"])?;
    systemctl_user(&["restart", "catalerum-agent.service"])?;

    println!("installed and started: {}", unit_path.display());
    println!("  status: systemctl --user status catalerum-agent");
    println!("  logs:   journalctl --user -u catalerum-agent -f");
    println!("  remove: systemctl --user disable --now catalerum-agent");
    let user = std::env::var("USER").unwrap_or_else(|_| "<user>".into());
    println!("to keep it running while logged out: loginctl enable-linger {user}");
    Ok(())
}

/// Run one `systemctl --user …` command, failing on a non-zero exit.
#[cfg(target_os = "linux")]
fn systemctl_user(args: &[&str]) -> Result<()> {
    let status = std::process::Command::new("systemctl")
        .arg("--user")
        .args(args)
        .status()
        .context("running systemctl (is this a systemd system with a user session?)")?;
    anyhow::ensure!(
        status.success(),
        "systemctl --user {} failed",
        args.join(" ")
    );
    Ok(())
}

/// Print the effective config with the token redacted.
fn show(path: &std::path::Path) -> Result<()> {
    let mut config = Config::load(path)?;
    config.token = "<redacted>".to_string();
    println!("config: {}", path.display());
    println!("{}", config.to_toml()?);
    let caps = AgentState::new(config).capabilities();
    println!("platform: {} ({})", caps.platform.label(), caps.arch);
    println!("sandbox:  {}", caps.sandbox.label());
    Ok(())
}

/// Load the config and run the connect/serve loop until Ctrl-C.
async fn run(path: &std::path::Path) -> Result<()> {
    let config = Config::load(path).with_context(|| {
        format!(
            "loading config {} — run `catalerum-agent enroll …` first",
            path.display()
        )
    })?;
    // Warn loudly if any served directory doesn't exist yet (a common misconfig).
    for grant in &config.dir_grants() {
        if !std::path::Path::new(&grant.path).exists() {
            tracing::warn!(dir = %grant.path, "served directory does not exist");
        }
    }
    let state = Arc::new(AgentState::new(config));
    log_summary(&state.capabilities());

    tokio::select! {
        _ = client::run(state) => {}
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("received Ctrl-C; shutting down");
        }
    }
    Ok(())
}

/// A one-line summary of what this daemon will serve, at startup.
fn log_summary(caps: &catalerum_core::computer::ComputerCapabilities) {
    let dirs: Vec<String> = caps
        .dirs
        .iter()
        .map(|d: &DirGrant| {
            format!(
                "{} ({})",
                d.path,
                if d.mode.can_write() { "rw" } else { "ro" }
            )
        })
        .collect();
    tracing::info!(
        platform = caps.platform.label(),
        sandbox = caps.sandbox.label(),
        desktop = caps.desktop,
        exec_policy = ?caps.exec_policy,
        dirs = ?dirs,
        "catalerum-agent starting"
    );
}

/// Parse the exec-policy string.
fn parse_exec_policy(s: &str) -> Result<ExecPolicy> {
    Ok(match s.trim().to_ascii_lowercase().as_str() {
        "auto" => ExecPolicy::Auto,
        "always_ask" | "ask" => ExecPolicy::AlwaysAsk,
        "always_allow" | "allow" => ExecPolicy::AlwaysAllow,
        "deny" | "none" => ExecPolicy::Deny,
        other => anyhow::bail!(
            "unknown exec policy `{other}` (expected auto / always_ask / always_allow / deny)"
        ),
    })
}
