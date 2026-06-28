//! The reconcile loop (SOUL §20): given a `WorkspaceSandbox` CR, converge a
//! per-workspace namespace with a hardened sandbox Pod, `/work` PVC, quota,
//! NetworkPolicy, and the API's exec RoleBinding; idle-suspend via `replicas:0`;
//! and clean the namespace on delete (finalizer-driven). All writes are
//! server-side apply, so re-running is idempotent and drift self-heals.

use std::sync::Arc;
use std::time::Duration;

use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{
    LimitRange, Namespace, PersistentVolumeClaim, Pod, ResourceQuota,
};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use k8s_openapi::api::rbac::v1::{Role, RoleBinding};
use kube::api::{DeleteParams, ListParams, Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::runtime::finalizer::{finalizer, Event as Finalizer};
use kube::{Api, Client, Resource, ResourceExt};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

use catalerum_k8s::crd::{Phase, WorkspaceSandbox};
use catalerum_k8s::{sandbox_namespace, FINALIZER, LABEL_WORKSPACE, MANAGEMENT_NAMESPACE};

use crate::resources::{self, Resources, DEFAULT_IMAGE};

/// Reconcile errors.
#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("kube error: {0}")]
    Kube(#[from] kube::Error),
    #[error("finalizer error: {0}")]
    Finalizer(String),
}

/// Shared reconcile context.
pub struct Ctx {
    pub client: Client,
    /// ServiceAccount name the API runs as (per-ns exec grant subject).
    pub api_service_account: String,
    /// Namespace that ServiceAccount lives in.
    pub api_namespace: String,
}

/// Entry point wrapped in the finalizer so cleanup runs before the CR is removed.
pub async fn reconcile(wsb: Arc<WorkspaceSandbox>, ctx: Arc<Ctx>) -> Result<Action, Error> {
    let api: Api<WorkspaceSandbox> = Api::namespaced(ctx.client.clone(), MANAGEMENT_NAMESPACE);
    finalizer(&api, FINALIZER, wsb, |event| async {
        match event {
            Finalizer::Apply(w) => apply(w, ctx.clone()).await,
            Finalizer::Cleanup(w) => cleanup(w, ctx.clone()).await,
        }
    })
    .await
    .map_err(|e| Error::Finalizer(e.to_string()))
}

/// Server-side apply a full manifest under our field manager (idempotent).
async fn ssa<K>(api: &Api<K>, name: &str, pp: &PatchParams, value: Value) -> Result<(), Error>
where
    K: Resource<DynamicType = ()> + Clone + DeserializeOwned + std::fmt::Debug,
{
    api.patch(name, pp, &Patch::Apply(value)).await?;
    Ok(())
}

/// Converge all per-workspace objects, then patch status.
async fn apply(wsb: Arc<WorkspaceSandbox>, ctx: Arc<Ctx>) -> Result<Action, Error> {
    if wsb.spec.paused {
        return Ok(Action::await_change());
    }
    let client = &ctx.client;
    let id = wsb.spec.workspace_id.clone();
    let ns = sandbox_namespace(&id);
    let pp = PatchParams::apply("catalerum-operator").force();

    // Namespace (cluster-scoped) first, then everything inside it.
    let ns_api: Api<Namespace> = Api::all(client.clone());
    ssa(&ns_api, &ns, &pp, resources::namespace(&ns, &id)).await?;

    let rq: Api<ResourceQuota> = Api::namespaced(client.clone(), &ns);
    ssa(
        &rq,
        "catalerum-quota",
        &pp,
        resources::resource_quota(&ns, &id),
    )
    .await?;
    let lr: Api<LimitRange> = Api::namespaced(client.clone(), &ns);
    ssa(
        &lr,
        "catalerum-limits",
        &pp,
        resources::limit_range(&ns, &id),
    )
    .await?;
    let np: Api<NetworkPolicy> = Api::namespaced(client.clone(), &ns);
    ssa(
        &np,
        "catalerum-netpol",
        &pp,
        resources::network_policy(&ns, &id, wsb.spec.network_policy),
    )
    .await?;
    let pvc: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), &ns);
    ssa(
        &pvc,
        "work",
        &pp,
        resources::pvc(
            &ns,
            &id,
            &wsb.spec.work_volume_size,
            wsb.spec.storage_class.as_deref(),
        ),
    )
    .await?;
    // The API's exec grant MUST exist before we mark Ready (so it never races).
    let role: Api<Role> = Api::namespaced(client.clone(), &ns);
    ssa(
        &role,
        "catalerum-api-exec",
        &pp,
        resources::api_exec_role(&ns, &id),
    )
    .await?;
    let rb: Api<RoleBinding> = Api::namespaced(client.clone(), &ns);
    ssa(
        &rb,
        "catalerum-api-exec",
        &pp,
        resources::api_exec_rolebinding(&ns, &id, &ctx.api_service_account, &ctx.api_namespace),
    )
    .await?;

    // Hard TTL: tear the whole sandbox down (namespace cascades PVC/Deployment/…).
    if wsb.spec.hard_ttl_seconds > 0 && idle_beyond(&wsb, wsb.spec.hard_ttl_seconds) {
        delete_namespace(&ns_api, &ns).await?;
        patch_status(client, &wsb, Phase::Terminating, &ns, None).await?;
        return Ok(Action::requeue(Duration::from_secs(30)));
    }

    // Soft idle suspend → replicas:0 (keeps the PVC); the API's lastActivity
    // patch fires a watch event that scales us back to 1.
    let replicas = if should_suspend(&wsb) { 0 } else { 1 };
    let image = wsb
        .spec
        .image
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_IMAGE.to_string());
    let res = Resources {
        cpu_request: wsb.spec.cpu_request.clone(),
        cpu_limit: wsb.spec.cpu_limit.clone(),
        memory_request: wsb.spec.memory_request.clone(),
        memory_limit: wsb.spec.memory_limit.clone(),
    };
    let dep_api: Api<Deployment> = Api::namespaced(client.clone(), &ns);
    ssa(
        &dep_api,
        "sandbox",
        &pp,
        resources::deployment(&ns, &id, &image, &res, replicas, &wsb.spec.env),
    )
    .await?;

    // Phase from observed readiness.
    let phase = if replicas == 0 {
        Phase::Suspended
    } else {
        let available = dep_api
            .get("sandbox")
            .await
            .ok()
            .and_then(|d| d.status)
            .and_then(|s| s.available_replicas)
            .unwrap_or(0);
        if available >= 1 {
            Phase::Ready
        } else {
            Phase::Provisioning
        }
    };
    let pod = if phase == Phase::Ready {
        ready_pod(client, &ns, &id).await
    } else {
        None
    };
    patch_status(client, &wsb, phase, &ns, pod).await?;

    // Periodic requeue enforces the idle→suspend transition (no event fires while idle).
    Ok(Action::requeue(Duration::from_secs(45)))
}

/// Delete the per-workspace namespace (cascades all children). `NotFound`-tolerant.
async fn cleanup(wsb: Arc<WorkspaceSandbox>, ctx: Arc<Ctx>) -> Result<Action, Error> {
    let ns = sandbox_namespace(&wsb.spec.workspace_id);
    let ns_api: Api<Namespace> = Api::all(ctx.client.clone());
    delete_namespace(&ns_api, &ns).await?;
    Ok(Action::await_change())
}

async fn delete_namespace(ns_api: &Api<Namespace>, ns: &str) -> Result<(), Error> {
    match ns_api.delete(ns, &DeleteParams::default()).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(e)) if e.code == 404 => Ok(()),
        Err(e) => Err(Error::Kube(e)),
    }
}

/// Build + apply the status subresource. `lastActivity` is only *initialized*
/// (never bumped) here, so the operator's reconciles don't reset the idle clock —
/// only the API's exec/attach patches advance it.
async fn patch_status(
    client: &Client,
    wsb: &WorkspaceSandbox,
    phase: Phase,
    ns: &str,
    pod: Option<String>,
) -> Result<(), Error> {
    let api: Api<WorkspaceSandbox> = Api::namespaced(client.clone(), MANAGEMENT_NAMESPACE);
    let name = wsb.name_any();
    let mut status = json!({
        "phase": phase,
        "namespace": ns,
        "podName": pod,
        "observedGeneration": wsb.meta().generation,
    });
    if wsb
        .status
        .as_ref()
        .and_then(|s| s.last_activity.as_ref())
        .is_none()
    {
        status["lastActivity"] = json!(now_rfc3339());
    }
    api.patch_status(
        &name,
        &PatchParams::default(),
        &Patch::Merge(json!({ "status": status })),
    )
    .await?;
    Ok(())
}

/// Seconds since `status.lastActivity`, if set + parseable.
fn idle_seconds(wsb: &WorkspaceSandbox) -> Option<i64> {
    let la = wsb.status.as_ref()?.last_activity.as_ref()?;
    let t = chrono::DateTime::parse_from_rfc3339(la).ok()?;
    Some((chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds())
}

fn idle_beyond(wsb: &WorkspaceSandbox, ttl: u64) -> bool {
    idle_seconds(wsb).is_some_and(|s| s >= 0 && (s as u64) >= ttl)
}

fn should_suspend(wsb: &WorkspaceSandbox) -> bool {
    let ttl = wsb.spec.idle_ttl_seconds;
    ttl != 0 && idle_beyond(wsb, ttl)
}

/// The name of a Running pod in `ns` carrying the workspace label, if any.
async fn ready_pod(client: &Client, ns: &str, id: &str) -> Option<String> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), ns);
    let lp = ListParams::default().labels(&format!("{LABEL_WORKSPACE}={id}"));
    let list = pods.list(&lp).await.ok()?;
    list.items.into_iter().find_map(|p| {
        let running = p.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Running");
        running.then(|| p.name_any())
    })
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// On a recoverable error, requeue with a short backoff.
pub fn error_policy(_obj: Arc<WorkspaceSandbox>, err: &Error, _ctx: Arc<Ctx>) -> Action {
    tracing::warn!(error = %err, "reconcile failed; requeuing");
    Action::requeue(Duration::from_secs(15))
}
