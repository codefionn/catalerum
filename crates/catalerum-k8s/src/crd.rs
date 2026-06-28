//! The `WorkspaceSandbox` custom resource (SOUL §20).
//!
//! One CR per catalerum workspace, living in the management namespace
//! (`catalerum-system`). The operator reconciles each into a per-workspace
//! namespace with a hardened sandbox Pod, a persistent `/work` PVC, a
//! ResourceQuota/LimitRange, a NetworkPolicy, and the API's exec RoleBinding.
//!
//! Timestamps are RFC3339 strings (not `chrono` types) so the schema derives
//! without pulling a schemars↔chrono feature; the API writes
//! `status.lastActivity` with a plain string patch.

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Desired state of a workspace's sandbox. The operator owns the cluster objects;
/// the API owns `status.lastActivity` (which drives idle GC).
#[derive(CustomResource, Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
#[kube(
    group = "catalerum.dev",
    version = "v1alpha1",
    kind = "WorkspaceSandbox",
    plural = "workspacesandboxes",
    shortname = "wsb",
    namespaced,
    status = "WorkspaceSandboxStatus",
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Pod","type":"string","jsonPath":".status.podName"}"#,
    printcolumn = r#"{"name":"Workspace","type":"string","jsonPath":".spec.workspaceId"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceSandboxSpec {
    /// Workspace UUID (lowercased). Drives the namespace/PVC/Deployment names.
    pub workspace_id: String,
    /// Container image; `None` → the operator default (`debian:stable-slim`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// CPU request, e.g. `"500m"` (`None` → operator default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_request: Option<String>,
    /// CPU limit, e.g. `"2"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_limit: Option<String>,
    /// Memory request, e.g. `"256Mi"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_request: Option<String>,
    /// Memory limit, e.g. `"1Gi"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_limit: Option<String>,
    /// Persistent `/work` PVC size, e.g. `"10Gi"`.
    pub work_volume_size: String,
    /// StorageClass for the `/work` PVC (`None` → the cluster default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_class: Option<String>,
    /// Network posture (default `Full`).
    #[serde(default)]
    pub network_policy: NetworkPolicyMode,
    /// Scale the Pod to 0 after this many idle seconds (`0` → never). `/work`
    /// persists; the API's `lastActivity` patch resumes it.
    #[serde(default)]
    pub idle_ttl_seconds: u64,
    /// Hard-delete the whole sandbox (namespace + PVC) after this many idle
    /// seconds (`0` → never).
    #[serde(default)]
    pub hard_ttl_seconds: u64,
    /// Extra environment for the sandbox container.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Pause reconciliation (the operator leaves objects untouched).
    #[serde(default)]
    pub paused: bool,
}

/// Network posture for a workspace sandbox.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum NetworkPolicyMode {
    /// Full internet egress (cloud metadata blocked), same-namespace ingress only.
    #[default]
    Full,
    /// No internet egress (DNS + same-namespace only).
    Isolated,
}

/// Observed state of a workspace sandbox.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceSandboxStatus {
    #[serde(default)]
    pub phase: Phase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pvc_name: Option<String>,
    /// RFC3339 timestamp the API stamps on each exec/attach (drives idle GC).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
}

/// Reconcile phase.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum Phase {
    #[default]
    Pending,
    Provisioning,
    Ready,
    Suspended,
    Terminating,
    Failed,
}

/// A status condition (mirrors the k8s convention).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Condition {
    /// e.g. `Ready`, `PvcBound`, `NetworkPolicyApplied`.
    pub r#type: String,
    /// `True` / `False` / `Unknown`.
    pub status: String,
    pub reason: String,
    pub message: String,
    /// RFC3339 timestamp.
    pub last_transition_time: String,
}
