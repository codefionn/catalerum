//! PID 1 for the all-in-one image. All internal services bind loopback; nginx is
//! the sole public listener and exposes the SPA plus `/api/*` on one origin.

use std::path::Path;
use std::process::Stdio;

use anyhow::{bail, Context};
use ring::rand::SecureRandom as _;
use tokio::process::{Child, Command};

struct Service {
    name: &'static str,
    child: Child,
}

fn command(name: &'static str, program: &str, args: &[&str]) -> anyhow::Result<Service> {
    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("starting {name} ({program})"))?;
    Ok(Service { name, child })
}

fn control_token() -> anyhow::Result<String> {
    if let Ok(token) = std::env::var("LLMLEAF_CONTROL_TOKEN") {
        if !token.trim().is_empty() {
            return Ok(token);
        }
    }
    let mut bytes = [0_u8; 32];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("failed to generate llmleaf control token"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

async fn stop_all(services: &mut [Service]) {
    for service in services.iter_mut().rev() {
        if let Err(error) = service.child.start_kill() {
            tracing::warn!(service = service.name, %error, "could not signal child");
        }
    }
    for service in services.iter_mut().rev() {
        let _ = service.child.wait().await;
    }
}

#[cfg(unix)]
async fn shutdown_signal() -> anyhow::Result<()> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("installing SIGTERM handler")?;
    tokio::select! {
        signal = tokio::signal::ctrl_c() => signal.context("installing Ctrl-C handler"),
        _ = terminate.recv() => Ok(()),
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() -> anyhow::Result<()> {
    tokio::signal::ctrl_c()
        .await
        .context("installing Ctrl-C handler")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    for directory in ["/data", "/data/qdrant", "/files", "/work", "/tmp/catalerum"] {
        tokio::fs::create_dir_all(directory)
            .await
            .with_context(|| format!("creating {directory}"))?;
    }
    for required in [
        "/usr/local/bin/catalerum",
        "/usr/local/bin/llmleaf",
        "/usr/local/bin/catalerum-preview-service",
        "/qdrant/qdrant",
        "/usr/sbin/nginx",
    ] {
        if !Path::new(required).exists() {
            bail!("all-in-one image is incomplete: missing {required}");
        }
    }

    let token = control_token()?;
    std::env::set_var("LLMLEAF_CONTROL_TOKEN", &token);
    std::env::set_var("CATALERUM_LLM__CONTROL_TOKEN", &token);
    let mut services = vec![
        command(
            "qdrant",
            "/qdrant/qdrant",
            &["--config-path", "/etc/catalerum/qdrant.yaml"],
        )?,
        command(
            "llmleaf",
            "/usr/local/bin/llmleaf",
            &["/etc/catalerum/llmleaf.toml"],
        )?,
        command("preview", "/usr/local/bin/catalerum-preview-service", &[])?,
        command(
            "api",
            "/usr/local/bin/catalerum",
            &["--config", "/etc/catalerum/all-in-one.toml"],
        )?,
        command("frontend router", "/usr/sbin/nginx", &["-g", "daemon off;"])?,
    ];

    tracing::info!("all-in-one services started; public listener is :8080");
    tokio::select! {
        signal = shutdown_signal() => {
            signal?;
            tracing::info!("shutdown requested");
        }
        outcome = async {
            loop {
                for service in &mut services {
                    if let Some(status) = service.child.try_wait()? {
                        return Ok::<_, std::io::Error>((service.name, status));
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        } => {
            let (name, status) = outcome?;
            tracing::error!(service = name, %status, "required child exited");
        }
    }
    stop_all(&mut services).await;
    Ok(())
}
