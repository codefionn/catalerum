//! The Kubernetes executor backend (SOUL §20): one Pod per interactive session,
//! driven via `kubectl`. This is a **thin** integration — `kubectl run` to start
//! a keep-alive Pod, `kubectl exec -it` for the PTY (reusing the shared
//! [`SessionStore`]), `kubectl delete` on close — rather than a full kube-rs/CRD
//! operator; the agent-facing behavior is identical and it needs no in-cluster
//! controller. Requires `kubectl` + a configured kubeconfig in the API
//! environment.
//!
//! Files live **in the Pod** (no host bind-mount), so a k8s session has no
//! `host_dir` and is not flushable to object storage via `persist` yet — that
//! needs `kubectl cp`, a documented follow-up. Open/write/read/resize/close work.
//!
//! Pods are hardened to the container backend's level via a `--overrides`
//! strategic merge (see [`KubernetesExecutor::security_overrides`]): all
//! capabilities dropped, privilege escalation blocked, RuntimeDefault seccomp,
//! the service-account token unmounted, and any CPU/memory limit applied.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use catalerum_core::error::{Error, Result};
use catalerum_core::provider::{
    ByteStream, CommandResult, CommandSpec, Executor, ResourceLimits, Session, SessionSpec,
};
use serde_json::{json, Value};

use crate::pty::SessionStore;

/// Default wall-clock timeout for one-shot `run`.
const DEFAULT_TIMEOUT_SECS: u64 = 60;
/// How long to wait for a session Pod to become Ready.
const POD_READY_TIMEOUT_SECS: u64 = 60;

/// Drives `kubectl` to run interactive Pod sessions + one-shot commands
/// (SOUL §20). Cloneable (shares the session/Pod registries).
#[derive(Clone, Debug)]
pub struct KubernetesExecutor {
    kubectl: String,
    namespace: String,
    image: String,
    sessions: SessionStore,
    /// `session id → pod name`, so a session's Pod is deleted on close.
    pods: Arc<Mutex<HashMap<String, String>>>,
}

impl KubernetesExecutor {
    /// A k8s backend creating Pods in `namespace` from a default `image`.
    #[must_use]
    pub fn new(namespace: impl Into<String>, image: impl Into<String>) -> Self {
        Self {
            kubectl: "kubectl".to_string(),
            namespace: namespace.into(),
            image: image.into(),
            sessions: SessionStore::new(),
            pods: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn ns(&self) -> &str {
        let n = self.namespace.trim();
        if n.is_empty() {
            "default"
        } else {
            n
        }
    }

    /// A DNS-1123-valid Pod name for a session (a v4 uuid is already lowercase
    /// hex + dashes).
    fn pod_name() -> String {
        format!("cat-term-{}", uuid::Uuid::new_v4())
    }

    /// Build the `kubectl run --overrides` JSON (paired with
    /// `--override-type=strategic`) that hardens a session/run Pod to the
    /// container backend's level — drop ALL caps + block privilege escalation —
    /// plus k8s-native lockdowns: no mounted service-account token, no service
    /// env-var injection, the RuntimeDefault seccomp profile, and any
    /// CPU/memory limit. Like the container backend it does **not** force
    /// `runAsNonRoot`, so root-default images (busybox) still start.
    ///
    /// `container` MUST be the generated container name (== the Pod name for
    /// `kubectl run NAME`) so the strategic merge lands on the real container
    /// instead of appending a second, image-less one. Requires kubectl ≥ 1.22
    /// (the `--override-type` flag); the network-isolation equivalent of the
    /// container backend's `--network none` needs a separate NetworkPolicy and
    /// is not yet emitted.
    fn security_overrides(container: &str, limits: &ResourceLimits) -> String {
        let mut ctr = serde_json::Map::new();
        ctr.insert("name".into(), json!(container));
        ctr.insert(
            "securityContext".into(),
            json!({
                "allowPrivilegeEscalation": false,
                "privileged": false,
                "capabilities": { "drop": ["ALL"] },
            }),
        );
        // limits == requests keeps the Pod in the Guaranteed QoS class.
        let mut lim = serde_json::Map::new();
        if let Some(cpu) = limits.cpu {
            lim.insert("cpu".into(), json!(cpu.to_string()));
        }
        if let Some(mem) = limits.memory_mb {
            lim.insert("memory".into(), json!(format!("{mem}Mi")));
        }
        if !lim.is_empty() {
            ctr.insert(
                "resources".into(),
                json!({ "limits": Value::Object(lim.clone()), "requests": Value::Object(lim) }),
            );
        }
        json!({
            "apiVersion": "v1",
            "spec": {
                "automountServiceAccountToken": false,
                "enableServiceLinks": false,
                "securityContext": { "seccompProfile": { "type": "RuntimeDefault" } },
                "containers": [Value::Object(ctr)],
            },
        })
        .to_string()
    }

    /// Run a `kubectl` subcommand to completion, capturing exit code +
    /// stdout/stderr. Capped + kill-on-drop (see [`crate::proc`]): a runaway
    /// command can't OOM the worker and a timed-out call reaps the client.
    async fn capture(&self, args: &[String]) -> Result<(i32, String, String)> {
        crate::proc::capture_capped(&self.kubectl, args, None).await
    }
}

#[async_trait]
impl Executor for KubernetesExecutor {
    async fn run(&self, cmd: CommandSpec) -> Result<CommandResult> {
        if cmd.argv.is_empty() {
            return Err(Error::invalid(
                "kubernetes run requires a non-empty argv (inline code unsupported)",
            ));
        }
        let name = Self::pod_name();
        let overrides = Self::security_overrides(&name, &cmd.limits);
        let mut args = vec![
            "run".into(),
            name.clone(),
            "-n".into(),
            self.ns().to_string(),
            "--restart=Never".into(),
            "--rm".into(),
            "-i".into(),
            "--image".into(),
            self.image.clone(),
            "--overrides".into(),
            overrides,
            "--override-type".into(),
            "strategic".into(),
        ];
        // Env rides `kubectl run --env` (it was silently dropped before);
        // everything after `--command --` is the container's argv.
        for (k, v) in &cmd.env {
            args.push("--env".into());
            args.push(format!("{k}={v}"));
        }
        args.push("--command".into());
        args.push("--".into());
        // `kubectl run` has no workdir flag — a requested cwd rides a positional
        // `sh -c` wrapper (never spliced into the script, so no quoting surface).
        if let Some(cwd) = cmd.cwd.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
            args.push("sh".into());
            args.push("-c".into());
            args.push("cd \"$1\" || exit 1; shift; exec \"$@\"".into());
            args.push("sh".into());
            args.push(cwd.into());
        }
        args.extend(cmd.argv.iter().cloned());

        let timeout = Duration::from_secs(cmd.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
        let collect = crate::proc::capture_capped(&self.kubectl, &args, cmd.stdin.clone());
        match tokio::time::timeout(timeout, collect).await {
            Ok(res) => {
                let (exit_code, stdout, stderr) = res?;
                Ok(CommandResult {
                    exit_code,
                    stdout,
                    stderr,
                    timed_out: false,
                })
            }
            Err(_) => {
                // The kubectl client was killed on drop, but the Pod keeps
                // running the command — delete it so a timed-out `run` doesn't
                // spin forever (best-effort).
                let _ = self
                    .capture(&[
                        "delete".into(),
                        "pod".into(),
                        name,
                        "-n".into(),
                        self.ns().to_string(),
                        "--now".into(),
                        "--ignore-not-found".into(),
                    ])
                    .await;
                Ok(CommandResult {
                    exit_code: -1,
                    stdout: String::new(),
                    stderr: format!("command timed out after {}s", timeout.as_secs()),
                    timed_out: true,
                })
            }
        }
    }

    async fn open_session(&self, spec: SessionSpec) -> Result<Session> {
        let image = spec
            .image
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| self.image.clone());
        let shell = spec
            .shell
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "/bin/sh".to_string());
        let pod = Self::pod_name();
        let ns = self.ns().to_string();
        let overrides = Self::security_overrides(&pod, &spec.limits);

        // Start a keep-alive Pod.
        let mut run_args = vec![
            "run".into(),
            pod.clone(),
            "-n".into(),
            ns.clone(),
            "--restart=Never".into(),
            "--image".into(),
            image,
            "--overrides".into(),
            overrides,
            "--override-type".into(),
            "strategic".into(),
            "--command".into(),
            "--".into(),
            "tail".into(),
            "-f".into(),
            "/dev/null".into(),
        ];
        for (k, v) in &spec.env {
            run_args.push("--env".into());
            run_args.push(format!("{k}={v}"));
        }
        let (code, _out, err) = self.capture(&run_args).await?;
        if code != 0 {
            return Err(Error::provider(format!("failed to create pod: {err}")));
        }

        // Wait for it to be Ready before exec.
        let (wcode, _wo, werr) = self
            .capture(&[
                "wait".into(),
                format!("pod/{pod}"),
                "-n".into(),
                ns.clone(),
                "--for=condition=Ready".into(),
                format!("--timeout={POD_READY_TIMEOUT_SECS}s"),
            ])
            .await?;
        if wcode != 0 {
            let _ = self
                .capture(&[
                    "delete".into(),
                    "pod".into(),
                    pod.clone(),
                    "-n".into(),
                    ns,
                    "--now".into(),
                ])
                .await;
            return Err(Error::provider(format!("pod never became ready: {werr}")));
        }

        // PTY-wrap `kubectl exec -it`. Files live in the Pod → no host_dir. The
        // shell may carry args (`bash --noprofile`) — split it, or the Pod would
        // exec a program literally named "bash --noprofile".
        let (shell_prog, shell_args) = crate::pty::split_command(&shell);
        let mut exec_args = vec![
            "exec".into(),
            "-it".into(),
            "-n".into(),
            ns.clone(),
            pod.clone(),
            "--".into(),
            shell_prog,
        ];
        exec_args.extend(shell_args);
        let pty_spec = SessionSpec {
            cols: spec.cols,
            rows: spec.rows,
            ..Default::default()
        };
        let session = match self
            .sessions
            .open(&self.kubectl, &exec_args, &pty_spec, false)
        {
            Ok(s) => s,
            Err(e) => {
                let _ = self
                    .capture(&[
                        "delete".into(),
                        "pod".into(),
                        pod,
                        "-n".into(),
                        ns,
                        "--now".into(),
                    ])
                    .await;
                return Err(e);
            }
        };
        if let Ok(mut g) = self.pods.lock() {
            g.insert(session.id.clone(), pod);
        }
        Ok(session)
    }

    async fn session_write(&self, session: &Session, data: Vec<u8>) -> Result<()> {
        self.sessions.write(&session.id, data).await
    }

    async fn session_read(&self, session: &Session, max_bytes: usize) -> Result<Vec<u8>> {
        self.sessions.read(&session.id, max_bytes)
    }

    async fn session_output(&self, session: &Session) -> Result<ByteStream> {
        self.sessions.output(&session.id)
    }

    async fn session_resize(&self, session: &Session, cols: u16, rows: u16) -> Result<()> {
        self.sessions.resize(&session.id, cols, rows)
    }

    async fn close_session(&self, session: &Session) -> Result<()> {
        let _ = self.sessions.close(&session.id).await;
        let pod = self
            .pods
            .lock()
            .ok()
            .and_then(|mut g| g.remove(&session.id));
        if let Some(pod) = pod {
            let _ = self
                .capture(&[
                    "delete".into(),
                    "pod".into(),
                    pod,
                    "-n".into(),
                    self.ns().to_string(),
                    "--now".into(),
                ])
                .await;
        }
        Ok(())
    }

    async fn reap(&self) -> Result<Vec<String>> {
        // A self-exited `kubectl exec` PTY leaves the keep-alive Pod running —
        // delete it too, not just the PTY entry.
        let dead = self.sessions.reap_exited()?;
        for id in &dead {
            let pod = self.pods.lock().ok().and_then(|mut g| g.remove(id));
            if let Some(pod) = pod {
                let _ = self
                    .capture(&[
                        "delete".into(),
                        "pod".into(),
                        pod,
                        "-n".into(),
                        self.ns().to_string(),
                        "--now".into(),
                    ])
                    .await;
            }
        }
        Ok(dead)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pod_names_are_dns_1123() {
        // Lowercase alphanumeric + '-', starting with a letter, ≤ 63 chars.
        for _ in 0..16 {
            let name = KubernetesExecutor::pod_name();
            assert!(name.len() <= 63, "pod name too long: {name}");
            assert!(name.starts_with("cat-term-"));
            assert!(
                name.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
                "non-DNS-1123 char in {name}"
            );
        }
    }

    #[test]
    fn namespace_defaults_to_default() {
        let exec = KubernetesExecutor::new("", "busybox");
        assert_eq!(exec.ns(), "default");
        let exec = KubernetesExecutor::new("ops", "busybox");
        assert_eq!(exec.ns(), "ops");
    }

    #[test]
    fn security_overrides_hardens_the_pod() {
        let limits = ResourceLimits {
            cpu: Some(2),
            memory_mb: Some(512),
            network: None,
        };
        let json: Value = serde_json::from_str(&KubernetesExecutor::security_overrides(
            "cat-term-x",
            &limits,
        ))
        .expect("valid override json");
        let spec = &json["spec"];
        assert_eq!(spec["automountServiceAccountToken"], json!(false));
        assert_eq!(spec["enableServiceLinks"], json!(false));
        assert_eq!(
            spec["securityContext"]["seccompProfile"]["type"],
            "RuntimeDefault"
        );

        let ctr = &spec["containers"][0];
        // The container name must match so the strategic merge lands on it.
        assert_eq!(ctr["name"], "cat-term-x");
        let sc = &ctr["securityContext"];
        assert_eq!(sc["allowPrivilegeEscalation"], json!(false));
        assert_eq!(sc["privileged"], json!(false));
        assert_eq!(sc["capabilities"]["drop"][0], "ALL");
        // limits == requests → Guaranteed QoS.
        assert_eq!(ctr["resources"]["limits"]["cpu"], "2");
        assert_eq!(ctr["resources"]["limits"]["memory"], "512Mi");
        assert_eq!(ctr["resources"]["requests"]["memory"], "512Mi");
    }

    #[test]
    fn security_overrides_omits_empty_resources_but_still_drops_caps() {
        let json: Value = serde_json::from_str(&KubernetesExecutor::security_overrides(
            "c",
            &ResourceLimits::default(),
        ))
        .expect("valid override json");
        let ctr = &json["spec"]["containers"][0];
        assert!(
            ctr.get("resources").is_none(),
            "no resources when no limits"
        );
        assert_eq!(ctr["securityContext"]["capabilities"]["drop"][0], "ALL");
    }
}
