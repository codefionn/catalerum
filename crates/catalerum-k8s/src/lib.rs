//! The `WorkspaceSandbox` custom resource (SOUL §20) — shared by the
//! `catalerum-operator` (which reconciles it into real cluster objects) and
//! `catalerum-api` (which would create/patch it). Kept in its own small crate so
//! the heavy `kube`/`k8s-openapi` dependency stays out of `catalerum-core` (which
//! is `wasm`-transitive and dependency-light).

pub mod crd;

pub use crd::{
    Condition, NetworkPolicyMode, Phase, WorkspaceSandbox, WorkspaceSandboxSpec,
    WorkspaceSandboxStatus,
};

/// CRD API group.
pub const GROUP: &str = "catalerum.dev";
/// CRD API version.
pub const VERSION: &str = "v1alpha1";
/// The finalizer the operator sets so it can clean cluster objects before the CR
/// is removed.
pub const FINALIZER: &str = "catalerum.dev/cleanup";
/// Label carrying the workspace id, stamped on every managed object so the
/// operator can map children back to their `WorkspaceSandbox` and clean up.
pub const LABEL_WORKSPACE: &str = "catalerum.dev/workspace";
/// `app.kubernetes.io/managed-by` value for objects the operator owns.
pub const MANAGED_BY: &str = "catalerum-operator";
/// The management namespace `WorkspaceSandbox` CRs live in.
pub const MANAGEMENT_NAMESPACE: &str = "catalerum-system";

/// The per-workspace namespace name the operator provisions (`catalerum-ws-<id>`,
/// matching the podman container name so the two backends are symmetric).
#[must_use]
pub fn sandbox_namespace(workspace_id: &str) -> String {
    format!("catalerum-ws-{workspace_id}")
}

/// The `WorkspaceSandbox` CR name for a workspace (lives in [`MANAGEMENT_NAMESPACE`]).
#[must_use]
pub fn cr_name(workspace_id: &str) -> String {
    format!("catalerum-ws-{workspace_id}")
}
