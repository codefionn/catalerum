//! `catalerum-operator` — the Kubernetes operator that reconciles
//! `WorkspaceSandbox` custom resources into per-workspace secure sandboxes
//! (SOUL §20).
//!
//! Subcommands:
//! - `crd` — print the `WorkspaceSandbox` CRD YAML (for `deploy/crd/…`).
//! - `run` (default) — run the controller against the ambient kubeconfig /
//!   in-cluster service account.

mod controller;
mod resources;

use anyhow::Context as _;
use futures::StreamExt;
use kube::runtime::{watcher, Controller};
use kube::{Api, Client, CustomResourceExt};
use std::sync::Arc;

use catalerum_k8s::{WorkspaceSandbox, MANAGEMENT_NAMESPACE};
use controller::{error_policy, reconcile, Ctx};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "run".to_string());
    if mode == "crd" {
        // Emit the CRD manifest (committed to deploy/crd/ and applied to the cluster).
        let crd = WorkspaceSandbox::crd();
        println!("{}", serde_yaml::to_string(&crd)?);
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,catalerum_operator=debug".into()),
        )
        .init();

    // rustls has no (or, under workspace feature unification, several) crypto
    // provider features enabled, so it cannot auto-pick a process-level
    // CryptoProvider and panics on first TLS use. Pin ring explicitly before
    // the client builds its HTTPS connector; an Err just means a provider was
    // already installed, which is equally fine.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let client = Client::try_default()
        .await
        .context("building the Kubernetes client (kubeconfig / in-cluster)")?;
    let api: Api<WorkspaceSandbox> = Api::namespaced(client.clone(), MANAGEMENT_NAMESPACE);
    // The exec-grant subject: which ServiceAccount (and namespace) the API pods
    // run as. Env-overridable so the API can live outside the management
    // namespace (e.g. a Helm release in its own app namespace).
    let api_service_account = std::env::var("CATALERUM_API_SERVICE_ACCOUNT")
        .unwrap_or_else(|_| resources::API_SERVICE_ACCOUNT.to_string());
    let api_namespace = std::env::var("CATALERUM_API_NAMESPACE")
        .unwrap_or_else(|_| resources::MANAGEMENT_NAMESPACE.to_string());
    let ctx = Arc::new(Ctx {
        client,
        api_service_account,
        api_namespace,
    });

    tracing::info!(
        namespace = MANAGEMENT_NAMESPACE,
        "catalerum-operator starting; watching WorkspaceSandbox resources"
    );
    Controller::new(api, watcher::Config::default())
        .shutdown_on_signal()
        .run(reconcile, error_policy, ctx)
        .for_each(|res| async move {
            match res {
                Ok((obj, _action)) => tracing::debug!(?obj, "reconciled"),
                Err(e) => tracing::warn!(error = %e, "reconcile loop error"),
            }
        })
        .await;
    Ok(())
}
