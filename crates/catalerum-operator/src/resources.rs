//! Desired-state builders for the cluster objects the operator reconciles per
//! workspace (SOUL §20). Each returns a full manifest as a `serde_json::Value`
//! for server-side apply; the hardening mirrors `catalerum-exec`'s `k8s.rs`.

use catalerum_k8s::crd::NetworkPolicyMode;
use catalerum_k8s::{LABEL_WORKSPACE, MANAGED_BY};
use serde_json::{json, Value};

/// The ServiceAccount + namespace the API runs as (for the per-ns exec grant).
/// Defaults only — `main` overrides them from `CATALERUM_API_SERVICE_ACCOUNT` /
/// `CATALERUM_API_NAMESPACE` when the API is deployed outside the management
/// namespace (e.g. a Helm release in its own app namespace).
pub const API_SERVICE_ACCOUNT: &str = "catalerum-api";
pub const MANAGEMENT_NAMESPACE: &str = "catalerum-system";
/// Mount point for the persistent `/work` volume.
const WORKDIR: &str = "/work";
/// Default sandbox image when the CR doesn't pin one. Deliberately a public,
/// arch-agnostic bare image; real deployments pin the batteries-included
/// `catalerum/catalerum-sandbox` (Dockerfile `runtime-sandbox`) via the CR's
/// `spec.image` / the API's `[exec.k8s].image` config.
pub const DEFAULT_IMAGE: &str = "docker.io/library/debian:stable-slim";

/// Common labels stamped on every managed object (so children map back to the CR).
fn labels(id: &str) -> Value {
    json!({
        LABEL_WORKSPACE: id,
        "app.kubernetes.io/managed-by": MANAGED_BY,
    })
}

/// The per-workspace Namespace (cluster-scoped).
pub fn namespace(ns: &str, id: &str) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Namespace",
        "metadata": { "name": ns, "labels": labels(id) },
    })
}

/// A ResourceQuota capping the per-namespace blast radius.
pub fn resource_quota(ns: &str, id: &str) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "ResourceQuota",
        "metadata": { "name": "catalerum-quota", "namespace": ns, "labels": labels(id) },
        "spec": { "hard": {
            "pods": "4",
            "requests.cpu": "4",
            "requests.memory": "8Gi",
            "limits.cpu": "8",
            "limits.memory": "16Gi",
            "persistentvolumeclaims": "2",
        }},
    })
}

/// A LimitRange giving containers a sane default request/limit.
pub fn limit_range(ns: &str, id: &str) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "LimitRange",
        "metadata": { "name": "catalerum-limits", "namespace": ns, "labels": labels(id) },
        "spec": { "limits": [{
            "type": "Container",
            "default": { "cpu": "1", "memory": "1Gi" },
            "defaultRequest": { "cpu": "250m", "memory": "256Mi" },
        }]},
    })
}

/// The NetworkPolicy implementing the [`NetworkPolicyMode`]. `Full` = all egress
/// except cloud metadata, same-namespace ingress only; `Isolated` drops the
/// internet egress (DNS + same-namespace only). `kubectl exec` is apiserver→
/// kubelet traffic and is never affected.
pub fn network_policy(ns: &str, id: &str, mode: NetworkPolicyMode) -> Value {
    // DNS to kube-dns is always allowed so name resolution works.
    let dns_egress = json!({
        "to": [{ "namespaceSelector": {} }],
        "ports": [{ "protocol": "UDP", "port": 53 }, { "protocol": "TCP", "port": 53 }],
    });
    let mut egress = vec![dns_egress];
    if matches!(mode, NetworkPolicyMode::Full) {
        // Full internet, minus the link-local metadata range (credential theft).
        egress.push(json!({
            "to": [{ "ipBlock": { "cidr": "0.0.0.0/0", "except": ["169.254.0.0/16"] } }],
        }));
    }
    json!({
        "apiVersion": "networking.k8s.io/v1",
        "kind": "NetworkPolicy",
        "metadata": { "name": "catalerum-netpol", "namespace": ns, "labels": labels(id) },
        "spec": {
            "podSelector": {},
            "policyTypes": ["Ingress", "Egress"],
            // Isolate cross-namespace inbound: only same-namespace pods may connect.
            "ingress": [{ "from": [{ "podSelector": {} }] }],
            "egress": egress,
        },
    })
}

/// The persistent `/work` PVC (RWO).
pub fn pvc(ns: &str, id: &str, size: &str, storage_class: Option<&str>) -> Value {
    let mut spec = json!({
        "accessModes": ["ReadWriteOnce"],
        "resources": { "requests": { "storage": size } },
    });
    if let Some(sc) = storage_class.filter(|s| !s.trim().is_empty()) {
        spec["storageClassName"] = json!(sc);
    }
    json!({
        "apiVersion": "v1",
        "kind": "PersistentVolumeClaim",
        "metadata": { "name": "work", "namespace": ns, "labels": labels(id) },
        "spec": spec,
    })
}

/// A Role granting the API exec into this namespace's sandbox pods.
pub fn api_exec_role(ns: &str, id: &str) -> Value {
    json!({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "Role",
        "metadata": { "name": "catalerum-api-exec", "namespace": ns, "labels": labels(id) },
        "rules": [
            { "apiGroups": [""], "resources": ["pods"], "verbs": ["get", "list"] },
            { "apiGroups": [""], "resources": ["pods/exec"], "verbs": ["create"] },
            { "apiGroups": [""], "resources": ["pods/log"], "verbs": ["get"] },
        ],
    })
}

/// Binds the API ServiceAccount (`api_sa` in `api_ns`) to the per-namespace
/// exec Role.
pub fn api_exec_rolebinding(ns: &str, id: &str, api_sa: &str, api_ns: &str) -> Value {
    json!({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "RoleBinding",
        "metadata": { "name": "catalerum-api-exec", "namespace": ns, "labels": labels(id) },
        "roleRef": {
            "apiGroup": "rbac.authorization.k8s.io",
            "kind": "Role",
            "name": "catalerum-api-exec",
        },
        "subjects": [{
            "kind": "ServiceAccount",
            "name": api_sa,
            "namespace": api_ns,
        }],
    })
}

/// Resource requests/limits for the sandbox container.
pub struct Resources {
    pub cpu_request: Option<String>,
    pub cpu_limit: Option<String>,
    pub memory_request: Option<String>,
    pub memory_limit: Option<String>,
}

/// The hardened sandbox Deployment (keep-alive `tail -f /dev/null`), `/work`
/// PVC-mounted. `replicas` is 0 when idle-suspended, else 1. Hardening mirrors
/// `k8s.rs`: drop ALL caps, no privilege escalation, RuntimeDefault seccomp,
/// service-account token unmounted, `fsGroup` so a non-root image can write `/work`.
///
/// `fsGroup` only works on volume plugins that honor it — hostPath-backed PVs
/// (k3s local-path!) silently ignore it, and a volume populated by an earlier
/// root-running image stays root-owned. So an init container (same image, uid 0,
/// only CHOWN/DAC_OVERRIDE) chowns `/work` to 1000 before the sandbox starts;
/// this requires the image to ship `sh` + `chown` (debian and the
/// catalerum-sandbox image both do).
pub fn deployment(
    ns: &str,
    id: &str,
    image: &str,
    resources: &Resources,
    replicas: i32,
    env: &std::collections::BTreeMap<String, String>,
) -> Value {
    let mut req = serde_json::Map::new();
    let mut lim = serde_json::Map::new();
    if let Some(c) = &resources.cpu_request {
        req.insert("cpu".into(), json!(c));
    }
    if let Some(m) = &resources.memory_request {
        req.insert("memory".into(), json!(m));
    }
    if let Some(c) = &resources.cpu_limit {
        lim.insert("cpu".into(), json!(c));
    }
    if let Some(m) = &resources.memory_limit {
        lim.insert("memory".into(), json!(m));
    }
    let mut res = serde_json::Map::new();
    if !req.is_empty() {
        res.insert("requests".into(), Value::Object(req));
    }
    if !lim.is_empty() {
        res.insert("limits".into(), Value::Object(lim));
    }
    let env_list: Vec<Value> = env
        .iter()
        .map(|(k, v)| json!({ "name": k, "value": v }))
        .collect();

    json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": { "name": "sandbox", "namespace": ns, "labels": labels(id) },
        "spec": {
            "replicas": replicas,
            "selector": { "matchLabels": { LABEL_WORKSPACE: id } },
            "template": {
                "metadata": { "labels": labels(id) },
                "spec": {
                    "automountServiceAccountToken": false,
                    "enableServiceLinks": false,
                    "securityContext": {
                        "seccompProfile": { "type": "RuntimeDefault" },
                        "fsGroup": 1000,
                    },
                    "initContainers": [{
                        "name": "chown-work",
                        "image": image,
                        "command": ["sh", "-c", "chown -R 1000:1000 /work"],
                        "securityContext": {
                            "runAsUser": 0,
                            "runAsGroup": 0,
                            "runAsNonRoot": false,
                            "allowPrivilegeEscalation": false,
                            "privileged": false,
                            "capabilities": { "drop": ["ALL"], "add": ["CHOWN", "DAC_OVERRIDE"] },
                        },
                        "volumeMounts": [{ "name": "work", "mountPath": WORKDIR }],
                    }],
                    "containers": [{
                        "name": "sandbox",
                        "image": image,
                        "command": ["tail", "-f", "/dev/null"],
                        "workingDir": WORKDIR,
                        "securityContext": {
                            "allowPrivilegeEscalation": false,
                            "privileged": false,
                            "capabilities": { "drop": ["ALL"] },
                        },
                        "resources": Value::Object(res),
                        "volumeMounts": [{ "name": "work", "mountPath": WORKDIR }],
                        "env": env_list,
                    }],
                    "volumes": [{
                        "name": "work",
                        "persistentVolumeClaim": { "claimName": "work" },
                    }],
                },
            },
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use catalerum_k8s::{WorkspaceSandbox, GROUP};
    use kube::CustomResourceExt;

    #[test]
    fn crd_has_the_expected_identity() {
        let crd = WorkspaceSandbox::crd();
        assert_eq!(crd.spec.group, GROUP);
        assert_eq!(crd.spec.names.kind, "WorkspaceSandbox");
        assert_eq!(crd.spec.names.plural, "workspacesandboxes");
        assert_eq!(crd.spec.versions[0].name, "v1alpha1");
    }

    #[test]
    fn full_network_allows_internet_minus_metadata_and_isolates_inbound() {
        let np = network_policy("catalerum-ws-x", "x", NetworkPolicyMode::Full);
        let egress = np["spec"]["egress"].as_array().unwrap();
        // DNS rule + internet rule.
        assert_eq!(egress.len(), 2);
        let internet = &egress[1]["to"][0]["ipBlock"];
        assert_eq!(internet["cidr"], "0.0.0.0/0");
        assert_eq!(internet["except"][0], "169.254.0.0/16");
        // Ingress is same-namespace only (cross-ns inbound denied).
        let ingress = np["spec"]["ingress"].as_array().unwrap();
        assert!(ingress[0]["from"][0].get("podSelector").is_some());
    }

    #[test]
    fn isolated_network_drops_internet_egress() {
        let np = network_policy("catalerum-ws-x", "x", NetworkPolicyMode::Isolated);
        // Only the DNS rule remains.
        assert_eq!(np["spec"]["egress"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn api_exec_rolebinding_targets_the_given_subject() {
        let rb = api_exec_rolebinding("catalerum-ws-x", "x", "catalerum-api", "catalerum");
        assert_eq!(rb["roleRef"]["name"], "catalerum-api-exec");
        let subject = &rb["subjects"][0];
        assert_eq!(subject["kind"], "ServiceAccount");
        assert_eq!(subject["name"], "catalerum-api");
        assert_eq!(subject["namespace"], "catalerum");
    }

    #[test]
    fn deployment_is_hardened() {
        let res = Resources {
            cpu_request: None,
            cpu_limit: None,
            memory_request: None,
            memory_limit: None,
        };
        let dep = deployment(
            "catalerum-ws-x",
            "x",
            "busybox",
            &res,
            1,
            &std::collections::BTreeMap::new(),
        );
        let pod = &dep["spec"]["template"]["spec"];
        assert_eq!(pod["automountServiceAccountToken"], false);
        assert_eq!(
            pod["securityContext"]["seccompProfile"]["type"],
            "RuntimeDefault"
        );
        let ctr = &pod["containers"][0];
        assert_eq!(ctr["securityContext"]["allowPrivilegeEscalation"], false);
        assert_eq!(ctr["securityContext"]["capabilities"]["drop"][0], "ALL");
        assert_eq!(ctr["command"][0], "tail");
        assert_eq!(ctr["volumeMounts"][0]["mountPath"], WORKDIR);
        // The chown init container is root but tightly capped: fsGroup alone
        // can't fix ownership on hostPath-backed PVs (k3s local-path).
        let init = &pod["initContainers"][0];
        assert_eq!(init["image"], "busybox");
        assert_eq!(init["securityContext"]["runAsUser"], 0);
        assert_eq!(init["securityContext"]["capabilities"]["drop"][0], "ALL");
        assert_eq!(init["securityContext"]["capabilities"]["add"][0], "CHOWN");
        assert_eq!(init["volumeMounts"][0]["mountPath"], WORKDIR);
    }
}
