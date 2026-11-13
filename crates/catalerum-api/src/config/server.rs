//! Server, deployment mode, telemetry and UI defaults.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::Secret;

/// Distributed tracing exporters. Both destinations are optional and may run
/// together; logs continue to be written to stderr regardless.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct TelemetryConfig {
    /// OpenTelemetry `service.name` resource attribute.
    pub service_name: String,
    /// Head sampling ratio in the inclusive range 0..=1.
    pub sample_ratio: f64,
    /// Vendor-neutral OTLP/HTTP exporter.
    pub otlp: OtelExporterConfig,
    /// Optional direct Langfuse OTLP/HTTP exporter.
    pub langfuse: LangfuseTelemetryConfig,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            service_name: "catalerum".to_string(),
            sample_ratio: 1.0,
            otlp: OtelExporterConfig::default(),
            langfuse: LangfuseTelemetryConfig::default(),
        }
    }
}

/// Which LLM payloads may be attached to exported generation spans. Metadata
/// (model, latency, token usage, cost, finish reason, errors) is always kept.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TelemetryContent {
    /// Export no prompt, completion, reasoning, tool arguments, or tool results.
    #[default]
    MetadataOnly,
    /// Export content after removing every system-role message.
    AllExceptSystemPrompts,
    /// Export the complete request and generated output, including system prompts.
    Everything,
}

impl std::str::FromStr for TelemetryContent {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "metadata-only" | "metadata" | "none" => Ok(Self::MetadataOnly),
            "all-except-system-prompts" | "without-system-prompts" => {
                Ok(Self::AllExceptSystemPrompts)
            }
            "everything" | "all" => Ok(Self::Everything),
            _ => Err(format!("unknown telemetry content mode: {value}")),
        }
    }
}

/// A generic OTLP/HTTP protobuf destination. The exporter appends `/v1/traces`
/// to `endpoint`, matching standard OTLP endpoint behavior.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct OtelExporterConfig {
    pub enabled: bool,
    pub endpoint: String,
    /// Static HTTP headers. Values are redacted by [`Secret`]'s `Debug` impl.
    pub headers: BTreeMap<String, Secret>,
    pub content: TelemetryContent,
}

impl Default for OtelExporterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: "http://localhost:4318".to_string(),
            headers: BTreeMap::new(),
            content: TelemetryContent::MetadataOnly,
        }
    }
}

/// Langfuse's native OTLP endpoint and Basic-auth credentials. `endpoint` is a
/// base OTLP URL; `/v1/traces` is appended by the exporter.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct LangfuseTelemetryConfig {
    pub enabled: bool,
    pub endpoint: String,
    pub public_key: Secret,
    pub secret_key: Secret,
    pub content: TelemetryContent,
}

impl Default for LangfuseTelemetryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: "https://cloud.langfuse.com/api/public/otel".to_string(),
            public_key: Secret::default(),
            secret_key: Secret::default(),
            content: TelemetryContent::MetadataOnly,
        }
    }
}

/// Emerged UIs — AI-authored declarative UIs (SOUL §5/§12). `handler_tools` is
/// the **server-defined** allow-list of tools a UI event handler (or its Boa
/// script) may invoke. It is the trust boundary that keeps an AI-authored UI
/// strictly **less** powerful than chat — re-checked before dispatch even for an
/// Owner whose grant is otherwise wildcard. It deliberately excludes
/// destructive (`delete_*`), egress (`fetch_url`), channel (`notify`), and code
/// (`run_command`) tools; an admin can widen it in config. (`run_javascript`'s
/// own nested `catalerum.callTool` bridge is disabled for UI-handler contexts
/// regardless, so listing it here cannot tunnel past this allow-list.)
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct UiConfig {
    /// Tools a UI event handler may call (capability-gated regardless).
    pub handler_tools: Vec<String>,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            handler_tools: [
                // Reads.
                "read_note",
                "list_notes",
                "query_structured",
                "search_files",
                "search_messages",
                "read_conversation",
                "read_event",
                "search_events",
                "kanban_next_task",
                "list_skills",
                "recall",
                // SQL against a workspace's external Postgres connections (SOUL §11).
                // Still capability-gated (`db:read@conn` / `db:write@conn`) and the
                // read path can never modify data, so it fits the safe-tool bar.
                "sql_query",
                // Safe writes (no delete / egress / channel / exec).
                "create_note",
                "edit_note",
                "kanban_create_board",
                "kanban_create_task",
                "kanban_move_task",
                "kanban_complete_task",
                "kanban_set_task_status",
                "kanban_edit_task",
                "create_calendar",
                "create_event",
                "update_event",
                "remember",
                "update_memory",
                "update_profile",
                // Per-App durable key/value store (SOUL §12/§29): where an emerged App
                // persists and reads its own data model so it outlives the session.
                // Reads gated on `ui:read` (every role), writes on `ui:write` (Member+,
                // base authority so a script may call them — never confirm-required).
                // From an App handler the namespace is forced to the firing App, so one
                // App can never reach another App's keys.
                "app_data_get",
                "app_data_list",
                "app_data_set",
                "app_data_delete",
                // Fire a named automation signal on demand (SOUL §11/§12) — lets an
                // emerged-UI button run a backend workflow. Safe: no delete / egress /
                // channel / exec, gated on `automation:write`, and each fired
                // automation still runs under its own §19 authority.
                "fire_trigger",
            ]
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
        }
    }
}

/// The deployment mode (SOUL §13/§18). One instance-level knob whose invariant
/// is: **mode changes presentation, defaults, and optimizations — never the
/// model.** Schema, API, authorization, and org/workspace tenancy are identical in
/// both; a single-user instance is a multi-tenant instance with one human in it,
/// and flipping the mode later is a config edit, not a migration.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentMode {
    /// Optimizes for one person (what `just dev` runs): the UI surfaces full
    /// per-user settings depth, hides member/role/SSO chrome, auto-selects the
    /// default organisation/workspace, and the runtime may cache around the sole
    /// user. Creation policies default to `members`. This is the default when the
    /// knob is unset — a fresh, zero-config instance is single-operator.
    #[default]
    SingleUser,
    /// Optimizes for a shared deployment: admins preconfigure defaults and members
    /// see fewer knobs, while member-management + SSO surfaces appear. Creation
    /// policies default to `admins`.
    MultiUser,
}

impl DeploymentMode {
    /// The lowercase token (`single_user` | `multi_user`) — the presentation-only
    /// value the web app reads to shape nav/settings chrome (SOUL §18).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            DeploymentMode::SingleUser => "single_user",
            DeploymentMode::MultiUser => "multi_user",
        }
    }

    /// Whether the runtime may cache around a **sole user** (SOUL §18/§29): true
    /// only in `single_user`, where one human means a per-workspace personalization
    /// cache (the profile snapshot) is coherent and process-local. `multi_user`
    /// never consults it (mode gates optimizations only, never correctness).
    #[must_use]
    pub fn caches_sole_user(self) -> bool {
        matches!(self, DeploymentMode::SingleUser)
    }

    /// The mode's default creation policy, applied to both the instance
    /// `organisation_creation` policy and a newly-created org's `workspace_creation`
    /// policy when neither is set explicitly: `Members` in single-user, `Admins`
    /// in multi-user (SOUL §18).
    #[must_use]
    pub fn default_creation_policy(self) -> catalerum_core::model::CreationPolicy {
        use catalerum_core::model::CreationPolicy;
        match self {
            DeploymentMode::SingleUser => CreationPolicy::Members,
            DeploymentMode::MultiUser => CreationPolicy::Admins,
        }
    }
}

/// The Axum API listen address and the externally-visible base URL used for
/// magic-link URLs (SOUL §17/§18).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Deployment mode (SOUL §18) — presentation/defaults/optimizations only,
    /// never schema/API/authz/tenancy. Defaults to `single_user` when unset.
    pub mode: DeploymentMode,
    /// Instance policy: who may create **new organisations**
    /// (`disabled` | `admins` | `members`, SOUL §18). Deny-by-default; unset falls
    /// back to the deployment mode's default (`members` in single-user, `admins`
    /// in multi-user) via [`effective_organisation_creation`](Self::effective_organisation_creation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organisation_creation: Option<catalerum_core::model::CreationPolicy>,
    /// Socket address the API binds, e.g. `0.0.0.0:8787`.
    pub listen: String,
    /// This pod's **peer-reachable IP** for cross-pod session forwarding (SOUL
    /// §16 M7): combined with `listen`'s port and announced on the bus registry
    /// so a peer can route a pod-local terminal request to its owner. On k8s,
    /// inject the downward-API pod IP (`CATALERUM_SERVER__POD_IP` from
    /// `status.podIP`). Empty (the default) auto-detects the primary local IP;
    /// forwarding degrades gracefully when no address can be determined.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pod_ip: String,
    /// Public base URL the API + magic links are rendered against. If unset the
    /// service falls back to `http://<listen>`.
    pub base_url: Option<String>,
    /// Public URL of the Leptos web workbench (the SPA origin). The dev
    /// magic-link redeem endpoint 302-redirects here with a one-time handoff
    /// code so dev login is a single click (SOUL §17). Defaults to
    /// `http://localhost:8080` (the `trunk serve` address). This origin is
    /// always on the CORS allow-list.
    pub web_url: String,
    /// Additional origins on the CORS allow-list (beyond `web_url`), e.g. a
    /// second SPA or an admin console served from another origin. The API
    /// answers cross-origin browser requests **only** from these origins
    /// (deny-by-default; non-browser clients are unaffected by CORS).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cors_extra_origins: Vec<String>,
    /// Secret that signs `download_link` tokens (SOUL §9) — the short-lived,
    /// single-file/-directory links the agent hands the user (`GET /download/{token}`).
    /// Set it (any string) so links survive a restart and verify across pods; leave
    /// it unset and each process signs with a fresh random key (links then die with
    /// the process — fine for single-pod dev).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_secret: Option<Secret>,
    /// Secret that signs **trigger-fire** links (SOUL §11/§12) — the short-lived
    /// public URLs (`POST /triggers/fire/{token}`) an external service uses to fire
    /// one named automation signal without a login. Set it (any string) so links
    /// survive a restart and verify across pods; leave it unset and each process
    /// signs with a fresh random key (links then die with the process — fine for
    /// single-pod dev). Independent of [`download_secret`](Self::download_secret) so
    /// the two can be rotated separately.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_secret: Option<Secret>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            mode: DeploymentMode::default(),
            organisation_creation: None,
            listen: "0.0.0.0:8787".to_string(),
            pod_ip: String::new(),
            base_url: None,
            web_url: "http://localhost:8080".to_string(),
            cors_extra_origins: Vec::new(),
            download_secret: None,
            trigger_secret: None,
        }
    }
}

impl ServerConfig {
    /// The effective instance `organisation_creation` policy (SOUL §18): the
    /// explicit config value, else the deployment mode's default (`members` in
    /// single-user, `admins` in multi-user). Deny-by-default gating consumes this.
    #[must_use]
    pub fn effective_organisation_creation(&self) -> catalerum_core::model::CreationPolicy {
        self.organisation_creation
            .unwrap_or_else(|| self.mode.default_creation_policy())
    }

    /// The default `workspace_creation` policy stamped onto a **newly-created**
    /// organisation (SOUL §18) — the deployment mode's default. Existing orgs keep
    /// their stored policy; this only seeds new ones at creation time.
    #[must_use]
    pub fn default_workspace_creation(&self) -> catalerum_core::model::CreationPolicy {
        self.mode.default_creation_policy()
    }

    /// The effective base URL: explicit `base_url` or `http://<listen>`.
    #[must_use]
    pub fn effective_base_url(&self) -> String {
        match &self.base_url {
            Some(b) => b.trim_end_matches('/').to_string(),
            None => format!("http://{}", self.listen),
        }
    }

    /// The web workbench (SPA) origin, trailing slash trimmed. The magic-link
    /// redeem endpoint 302-redirects here with a one-time handoff code.
    #[must_use]
    pub fn effective_web_url(&self) -> String {
        self.web_url.trim_end_matches('/').to_string()
    }

    /// The full CORS allow-list: the SPA origin plus any configured extras,
    /// trailing slashes trimmed, blanks dropped.
    #[must_use]
    pub fn cors_allowed_origins(&self) -> Vec<String> {
        std::iter::once(self.effective_web_url())
            .chain(
                self.cors_extra_origins
                    .iter()
                    .map(|o| o.trim().trim_end_matches('/').to_string()),
            )
            .filter(|o| !o.is_empty())
            .collect()
    }
}
