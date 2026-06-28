//! The Tool abstraction and registry (SOUL §3.3, §7).
//!
//! The LLM acts **only** through typed, scoped tools — it never touches a
//! provider, the database, or a shell directly. A [`Tool`] declares a name and a
//! JSON-Schema for its arguments, and is invoked asynchronously with a minimal
//! [`ToolContext`]. The single [`ToolRegistry`] is what the agent loop
//! (`catalerum-llm`), the API, and the MCP server register/dispatch against.
//!
//! Authorization happens at the API choke point (SOUL §19); a `Tool` impl is the
//! thin client of a scoped endpoint. The `ToolContext` carries the caller's
//! workspace/grant so an impl can pass them through.

use std::collections::HashMap;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use async_trait::async_trait;
use serde_json::Value as Json;

use crate::capability::Capability;
use crate::error::{Error, Result};
use crate::id::{AgentId, ConversationId, GrantId, UiDefinitionId, UserId, WorkspaceId};
use crate::llm::ToolSpec;

/// Reserved tool-result field carrying ephemeral native model input.
///
/// The agent loop removes this field before streaming/persisting the textual
/// tool result, then attaches its value to the next model turn.
pub const MODEL_MEDIA_RESULT_FIELD: &str = "__catalerum_model_media";

/// Minimal execution context handed to a [`Tool::invoke`] call (kept small for
/// M1, SOUL §7). Carries enough identity for the impl to authorize against the
/// API and to scope every query by workspace (SOUL §18).
///
/// `Debug` is hand-written (below) because [`gate`](Self::gate) is a trait object
/// that isn't `Debug`; the derive can't see through it.
#[derive(Clone, Default)]
pub struct ToolContext {
    /// The workspace the call is scoped to (always present in practice).
    pub workspace_id: Option<WorkspaceId>,
    /// The acting user, if the call is on behalf of a human.
    pub user_id: Option<UserId>,
    /// The acting agent, for automation/agent runs.
    pub agent_id: Option<AgentId>,
    /// The grant authorizing the call (SOUL §19).
    pub grant_id: Option<GrantId>,
    /// The caller's granted capabilities, for **per-action enforcement** at
    /// dispatch (SOUL §19). `None` disables the check (an internal/legacy caller
    /// trusted by construction); `Some(caps)` makes dispatch deny-by-default —
    /// a tool's [`required_capability`](Tool::required_capability) must be
    /// covered by one of `caps`.
    pub capabilities: Option<Vec<Capability>>,
    /// When set (from a grant's `dry_run` constraint, SOUL §19), dispatch
    /// **authorizes the call but does not execute it** — the tool's side effect is
    /// simulated, never committed. Lets a grant be exercised safely.
    pub dry_run: bool,
    /// An optional **programmable gate** layered *on top of* the capability check
    /// (a profile's tool guard). When `Some`, [`dispatch`](ToolRegistry::dispatch)
    /// asks it to classify the call (allow / deny) after the capability check but
    /// before the tool runs, and to classify the output afterwards. It can only
    /// **further restrict** — it never widens authority the capability check
    /// denied. Kept `None` for the gate's *own* re-entrant tool calls so a
    /// classifier can look things up without being re-classified (no recursion).
    pub gate: Option<Arc<dyn ToolGate>>,
    /// The conversation this call runs within, when it is an interactive chat turn
    /// (set by the WebSocket handler). Carries enough context for the `ask_user`
    /// tool (SOUL §7/§12) to persist a [`PendingQuestion`](crate::model::PendingQuestion)
    /// against the thread so the question form survives a reload/reconnect. `None`
    /// for a non-interactive run (an automation / channel worker) — there `ask_user`
    /// has no thread to surface a form on and degrades to "ask in prose".
    pub conversation_id: Option<ConversationId>,
    /// The emerged UI (App) whose event handler is firing this call, set **only**
    /// by the emerged-UI runtime (SOUL §12/§29). It scopes the per-App key/value
    /// tools (`app_data_*`) to the firing App's namespace so one App can never
    /// reach another App's keys — the namespace comes from *here*, not the caller's
    /// arguments. `None` for every other caller (chat, automation, MCP), where the
    /// `app_data_*` tools require an explicit `app` namespace argument instead.
    pub ui_id: Option<UiDefinitionId>,
    /// The registry this call was dispatched from, injected by
    /// [`dispatch`](ToolRegistry::dispatch) itself right before the tool runs —
    /// never set by a caller. It lets a tool that hosts **nested** tool calls
    /// (`run_javascript`'s `catalerum.callTool`) re-dispatch against the caller's
    /// *exact* tool surface (per-run derived registries included) under this same
    /// context, so every nested call passes the identical capability check + gate
    /// a directly-issued call would. `None` when the tool was invoked without
    /// going through `dispatch` (unit tests) — a nested-call host then fails
    /// closed.
    pub registry: Option<ToolRegistry>,
}

impl ToolContext {
    /// A context scoped to just a workspace.
    #[must_use]
    pub fn for_workspace(workspace_id: WorkspaceId) -> Self {
        Self {
            workspace_id: Some(workspace_id),
            ..Default::default()
        }
    }
}

impl std::fmt::Debug for ToolContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolContext")
            .field("workspace_id", &self.workspace_id)
            .field("user_id", &self.user_id)
            .field("agent_id", &self.agent_id)
            .field("grant_id", &self.grant_id)
            .field("capabilities", &self.capabilities)
            .field("dry_run", &self.dry_run)
            .field("gate", &self.gate.as_ref().map(|_| "<present>"))
            .field("conversation_id", &self.conversation_id)
            .field("ui_id", &self.ui_id)
            .field("registry", &self.registry.as_ref().map(|_| "<present>"))
            .finish()
    }
}

/// Where in a tool call's lifecycle a [`ToolGate`] is consulted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GatePhase {
    /// Before the tool runs — classifying the *requested call* (name + args).
    Call,
    /// After the tool ran — classifying the *result* it produced.
    Output,
}

/// A gate's ruling on one tool-call lifecycle event.
///
/// The `allow / deny / require-user-feedback` classification a profile authors is
/// resolved to this verdict *inside* the gate. "require-user-feedback" becomes a
/// [`Defer`](GateVerdict::Defer) (the call is held for the user's approval) when a
/// conversation is available, or a `Deny` (a headless run has no one to ask).
pub enum GateVerdict {
    /// Let the call proceed / pass the output through unchanged.
    Allow,
    /// Block it. On [`GatePhase::Call`] this becomes an
    /// [`Error::Denied`](crate::error::Error::Denied); on [`GatePhase::Output`]
    /// the real result is withheld and replaced by a policy marker.
    Deny {
        /// Human-readable reason, surfaced to the model (and, via the tool result,
        /// to the user).
        reason: String,
    },
    /// **Hold** the call for the user's approval (SOUL §19): the tool is **not
    /// run**; [`dispatch`](ToolRegistry::dispatch) returns `result` as the tool's
    /// output so the model reads "awaiting approval" and ends its turn. The gate
    /// has recorded a durable pending approval; the user's Approve re-runs the call.
    /// Only meaningful on [`GatePhase::Call`].
    Defer {
        /// The stand-in tool result (an `awaiting_approval` marker) the model sees.
        result: Json,
    },
}

/// A programmable authorization gate a profile can attach to its tool calls
/// (SOUL §19) — the runtime seam behind a per-profile **tool guard**.
///
/// Consulted by [`ToolRegistry::dispatch`] when [`ToolContext::gate`] is `Some`,
/// *after* the deny-by-default capability check, so it can only tighten authority.
/// The implementation (in `catalerum-api`) evaluates the profile's Boa JS and/or
/// LLM classifier and, for a "require-user-feedback" outcome, blocks awaiting the
/// user before returning a verdict.
#[async_trait]
pub trait ToolGate: Send + Sync {
    /// Classify one lifecycle event. `output` is `Some` only for
    /// [`GatePhase::Output`]. Returning [`GateVerdict::Deny`] blocks the call
    /// (call phase) or withholds the result (output phase).
    async fn review(
        &self,
        phase: GatePhase,
        tool: &dyn Tool,
        args: &Json,
        output: Option<&Json>,
        ctx: &ToolContext,
    ) -> GateVerdict;
}

/// A typed, scoped tool the LLM can call (SOUL §7).
///
/// Implementations are thin clients of a scoped API endpoint. `name` must be
/// unique within a [`ToolRegistry`]; `parameters_schema` is the JSON Schema
/// advertised to the model.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Stable, unique tool name (e.g. `create_note`, `query_graph`).
    fn name(&self) -> &str;

    /// One-line description shown to the model.
    fn description(&self) -> &str {
        ""
    }

    /// Model-specific description used by an agent run.
    ///
    /// Most tools are invariant and inherit [`description`](Self::description).
    /// A tool with optional native multimodal behavior can document only the
    /// media types the active model actually accepts.
    fn description_for(&self, _input_modalities: &[String]) -> String {
        self.description().to_string()
    }

    /// The capability this tool requires to run (SOUL §19), e.g.
    /// `Capability::new(Action::Write, Resource::domain("notes"))`. `None` means
    /// the tool is ungated. Enforced at [`ToolRegistry::dispatch`] when the
    /// caller's capabilities are known ([`ToolContext::capabilities`] is `Some`).
    fn required_capability(&self) -> Option<Capability> {
        None
    }

    /// JSON Schema describing the `args` object accepted by [`invoke`](Tool::invoke).
    fn parameters_schema(&self) -> Json;

    /// Render this tool as a [`ToolSpec`] for an LLM request.
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: self.parameters_schema(),
        }
    }

    /// Render this tool for a model with the supplied input modalities.
    fn spec_for(&self, input_modalities: &[String]) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: self.description_for(input_modalities),
            parameters: self.parameters_schema(),
        }
    }

    /// Execute the tool. `args` is the JSON arguments object the model emitted;
    /// the returned [`Json`] is the tool result appended to the conversation.
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json>;

    /// Execute with server-resolved model input capabilities.
    ///
    /// The default ignores the modalities. Binary-aware tools override this
    /// instead of accepting a spoofable hidden model argument.
    async fn invoke_for_model(
        &self,
        args: Json,
        ctx: &ToolContext,
        _input_modalities: &[String],
    ) -> Result<Json> {
        self.invoke(args, ctx).await
    }
}

/// The single registry of tools, shared by the agent loop, the API, and MCP
/// (SOUL §7). Cheap to clone (`Arc`-backed entries).
///
/// Most tools are **static** — registered once at startup, immutable thereafter.
/// A second, **dynamic** overlay holds tools added/removed *while the server runs*
/// — external MCP server tools managed by the `*_mcp_server` tools (SOUL §26). The
/// overlay is an `Arc<RwLock<…>>`, so it is **shared across every clone** of the
/// registry (every per-run/derived registry is built by `.clone()`); hot-adding a
/// tool there makes it visible to all of them at once, with no restart. Static
/// names take precedence over overlay names on lookup.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
    dynamic: Arc<RwLock<HashMap<String, Arc<dyn Tool>>>>,
}

impl ToolRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a static tool, replacing any existing static tool with the same
    /// name. Returns the displaced tool, if any.
    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Option<Arc<dyn Tool>> {
        self.tools.insert(tool.name().to_string(), tool)
    }

    /// Register a tool into the **runtime overlay** (SOUL §26) — visible to this
    /// registry and every clone of it, added without a restart. Replaces any
    /// overlay tool with the same name, returning it. `&self`: the overlay is
    /// interior-mutable (an `Arc<RwLock>`), so a hot-plug needs no `&mut`.
    pub fn register_dynamic(&self, tool: Arc<dyn Tool>) -> Option<Arc<dyn Tool>> {
        self.write_dynamic().insert(tool.name().to_string(), tool)
    }

    /// Remove a tool from the runtime overlay by name; returns it if present.
    pub fn unregister_dynamic(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.write_dynamic().remove(name)
    }

    /// Read guard over the overlay (recovers from a poisoned lock rather than
    /// panicking — a poisoned tool map shouldn't take down dispatch).
    fn read_dynamic(&self) -> RwLockReadGuard<'_, HashMap<String, Arc<dyn Tool>>> {
        self.dynamic.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write_dynamic(&self) -> RwLockWriteGuard<'_, HashMap<String, Arc<dyn Tool>>> {
        self.dynamic.write().unwrap_or_else(|e| e.into_inner())
    }

    /// Look up a tool by name — static entries first, then the runtime overlay.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        if let Some(t) = self.tools.get(name) {
            return Some(t.clone());
        }
        self.read_dynamic().get(name).cloned()
    }

    /// True if a tool with `name` is registered (static or overlay).
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name) || self.read_dynamic().contains_key(name)
    }

    /// All registered tool names (unordered).
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.tools.keys().map(String::as_str)
    }

    /// Number of registered tools.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// True if no tools are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Advertise the registered tools (optionally filtered to `allowed` names,
    /// e.g. an agent's restricted set) as [`ToolSpec`]s for an LLM request. Merges
    /// the static tools with the runtime overlay (SOUL §26), static winning on a
    /// name clash.
    #[must_use]
    pub fn specs(&self, allowed: Option<&[String]>) -> Vec<ToolSpec> {
        let pass = |t: &Arc<dyn Tool>| allowed.is_none_or(|a| a.iter().any(|n| n == t.name()));
        let mut specs: Vec<ToolSpec> = self
            .tools
            .values()
            .filter(|t| pass(t))
            .map(|t| t.spec())
            .collect();
        for t in self.read_dynamic().values() {
            if pass(t) && !self.tools.contains_key(t.name()) {
                specs.push(t.spec());
            }
        }
        specs
    }

    /// [`specs`](Self::specs), with descriptions tailored to the active model's
    /// advertised input modalities.
    #[must_use]
    pub fn specs_for_model(
        &self,
        allowed: Option<&[String]>,
        input_modalities: &[String],
    ) -> Vec<ToolSpec> {
        let pass = |t: &Arc<dyn Tool>| allowed.is_none_or(|a| a.iter().any(|n| n == t.name()));
        let mut specs: Vec<ToolSpec> = self
            .tools
            .values()
            .filter(|t| pass(t))
            .map(|t| t.spec_for(input_modalities))
            .collect();
        for t in self.read_dynamic().values() {
            if pass(t) && !self.tools.contains_key(t.name()) {
                specs.push(t.spec_for(input_modalities));
            }
        }
        specs
    }

    /// The [`ToolSpec`]s of exactly the named tools, in `names` order, skipping
    /// unknown names. The lookup half of **deferred tool advertising** (SOUL §7):
    /// the agent loop seeds a run with a small discovery subset and widens it with
    /// the specs of tools the model discovered via `search_tools`/`list_tools`.
    #[must_use]
    pub fn specs_for(&self, names: &[String]) -> Vec<ToolSpec> {
        names
            .iter()
            .filter_map(|n| self.get(n))
            .map(|t| t.spec())
            .collect()
    }

    /// [`specs_for`](Self::specs_for), with model-specific descriptions.
    #[must_use]
    pub fn specs_for_model_names(
        &self,
        names: &[String],
        input_modalities: &[String],
    ) -> Vec<ToolSpec> {
        names
            .iter()
            .filter_map(|n| self.get(n))
            .map(|t| t.spec_for(input_modalities))
            .collect()
    }

    /// The registered tools (optionally filtered to `allowed` names) grouped by
    /// the **domain** of their [`required_capability`](Tool::required_capability)
    /// — ungated tools land under `"general"`. Domains and names are sorted, so
    /// the output is deterministic (it feeds a cacheable system-prompt block for
    /// deferred tool advertising, SOUL §7). Merges static + overlay tools like
    /// [`specs`](Self::specs).
    #[must_use]
    pub fn domain_groups(&self, allowed: Option<&[String]>) -> Vec<(String, Vec<String>)> {
        let pass = |t: &Arc<dyn Tool>| allowed.is_none_or(|a| a.iter().any(|n| n == t.name()));
        let mut groups: HashMap<String, Vec<String>> = HashMap::new();
        let mut add = |t: &Arc<dyn Tool>| {
            let domain = t
                .required_capability()
                .map_or_else(|| "general".to_string(), |c| c.resource.domain);
            groups.entry(domain).or_default().push(t.name().to_string());
        };
        for t in self.tools.values().filter(|t| pass(t)) {
            add(t);
        }
        for t in self.read_dynamic().values() {
            if pass(t) && !self.tools.contains_key(t.name()) {
                add(t);
            }
        }
        let mut out: Vec<(String, Vec<String>)> = groups.into_iter().collect();
        for (_, names) in &mut out {
            names.sort();
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Dispatch a tool call by name. Returns [`Error::NotFound`] if no such tool
    /// is registered, or [`Error::Unauthorized`] if the caller's capabilities
    /// (when supplied) don't cover the tool's [`required_capability`].
    ///
    /// Enforcement is **deny-by-default but opt-in**: it runs only when
    /// `ctx.capabilities` is `Some` (a real caller's grant). A `None` context
    /// skips the check — for internal/trusted callers and to keep the workspace
    /// boundary the sole scope where capabilities aren't yet wired (SOUL §19).
    pub async fn dispatch(&self, name: &str, args: Json, ctx: &ToolContext) -> Result<Json> {
        self.dispatch_inner(name, args, ctx, None).await
    }

    /// Dispatch with trusted, server-resolved model input modalities. Ordinary
    /// callers use [`dispatch`](Self::dispatch), which supplies none and thus
    /// keeps optional binary behavior disabled.
    pub async fn dispatch_for_model(
        &self,
        name: &str,
        args: Json,
        ctx: &ToolContext,
        input_modalities: &[String],
    ) -> Result<Json> {
        self.dispatch_inner(name, args, ctx, Some(input_modalities))
            .await
    }

    async fn dispatch_inner(
        &self,
        name: &str,
        args: Json,
        ctx: &ToolContext,
        input_modalities: Option<&[String]>,
    ) -> Result<Json> {
        let tool = self.get(name).ok_or(Error::NotFound)?;
        if let (Some(caps), Some(required)) = (&ctx.capabilities, tool.required_capability()) {
            if !caps.iter().any(|held| held.covers(&required)) {
                return Err(Error::Unauthorized(format!(
                    "tool `{name}` requires {}:{} which the caller's grant does not cover",
                    required.resource.domain,
                    action_token(required.action),
                )));
            }
        }
        // A profile's programmable tool guard (SOUL §19) runs *after* the capability
        // check, so it can only tighten authority. A call-phase deny short-circuits
        // before the side effect fires (the model sees an error tool result); a
        // defer holds the call for the user's approval, returning a stand-in result
        // WITHOUT running the tool (the model reads "awaiting approval" and stops).
        if let Some(gate) = &ctx.gate {
            match gate
                .review(GatePhase::Call, tool.as_ref(), &args, None, ctx)
                .await
            {
                GateVerdict::Allow => {}
                GateVerdict::Deny { reason } => return Err(Error::Denied(reason)),
                GateVerdict::Defer { result } => return Ok(result),
            }
        }
        // §19 dry-run: the call is authorized (above), but a dry-run grant simulates
        // it — the tool's side effect is never committed. Returns what *would* run.
        if ctx.dry_run {
            return Ok(serde_json::json!({ "dry_run": true, "tool": name, "args": args }));
        }
        // Hand the tool the registry it was dispatched from, so a tool hosting
        // nested calls (`run_javascript`'s `catalerum.callTool`) re-dispatches
        // against the caller's exact tool surface under this same context — the
        // nested call passes the identical capability check + gate above.
        let invoke_ctx = ToolContext {
            registry: Some(self.clone()),
            ..ctx.clone()
        };
        let out = match input_modalities {
            Some(input_modalities) => {
                tool.invoke_for_model(args, &invoke_ctx, input_modalities)
                    .await?
            }
            None => tool.invoke(args, &invoke_ctx).await?,
        };
        // Output-phase guard: the side effect already ran, so a deny can't unrun it —
        // instead the real result is withheld and replaced by a policy marker the
        // model can read.
        if let Some(gate) = &ctx.gate {
            if let GateVerdict::Deny { reason } = gate
                .review(
                    GatePhase::Output,
                    tool.as_ref(),
                    &Json::Null,
                    Some(&out),
                    ctx,
                )
                .await
            {
                return Ok(serde_json::json!({ "policy_withheld": true, "reason": reason }));
            }
        }
        Ok(out)
    }
}

/// The lowercase token for an [`Action`](crate::capability::Action), for error
/// messages (`read`/`write`/`delete`/…).
fn action_token(action: crate::capability::Action) -> &'static str {
    use crate::capability::Action;
    match action {
        Action::Any => "*",
        Action::Read => "read",
        Action::Write => "write",
        Action::Delete => "delete",
        Action::Use => "use",
        Action::Run => "run",
        Action::Query => "query",
        Action::Search => "search",
        Action::Expose => "expose",
    }
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let dynamic: Vec<String> = self.read_dynamic().keys().cloned().collect();
        f.debug_struct("ToolRegistry")
            .field("tools", &self.tools.keys().collect::<Vec<_>>())
            .field("dynamic", &dynamic)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "returns its args"
        }
        fn parameters_schema(&self) -> Json {
            serde_json::json!({ "type": "object" })
        }
        async fn invoke(&self, args: Json, _ctx: &ToolContext) -> Result<Json> {
            Ok(args)
        }
    }

    /// A tool requiring `notes:write` — exercises capability enforcement.
    struct GatedTool;

    #[async_trait]
    impl Tool for GatedTool {
        fn name(&self) -> &str {
            "gated"
        }
        fn parameters_schema(&self) -> Json {
            serde_json::json!({ "type": "object" })
        }
        fn required_capability(&self) -> Option<Capability> {
            Some(Capability::new(
                crate::capability::Action::Write,
                crate::capability::Resource::domain("notes"),
            ))
        }
        async fn invoke(&self, _args: Json, _ctx: &ToolContext) -> Result<Json> {
            Ok(serde_json::json!({ "ok": true }))
        }
    }

    #[test]
    fn register_and_lookup() {
        let mut reg = ToolRegistry::new();
        assert!(reg.is_empty());
        reg.register(Arc::new(EchoTool));
        assert!(reg.contains("echo"));
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.specs(None).len(), 1);
        assert!(reg.specs(Some(&["other".to_string()])).is_empty());
    }

    #[test]
    fn dynamic_overlay_is_visible_through_clones_and_removable() {
        // The overlay (SOUL §26) is shared across clones, so a tool hot-registered
        // on one is dispatchable through another — the property the live MCP
        // manager relies on (it holds a clone; the agent loop holds another).
        let base = ToolRegistry::new();
        let derived = base.clone();
        base.register_dynamic(Arc::new(EchoTool));

        // Visible through the *clone*, by lookup + advertise + dispatch.
        assert!(derived.contains("echo"));
        assert!(derived.get("echo").is_some());
        assert!(derived.specs(None).iter().any(|s| s.name == "echo"));
        futures::executor::block_on(async {
            let out = derived
                .dispatch(
                    "echo",
                    serde_json::json!({ "x": 1 }),
                    &ToolContext::default(),
                )
                .await
                .unwrap();
            assert_eq!(out, serde_json::json!({ "x": 1 }));
        });

        // The `allowed` filter still applies to overlay tools.
        assert!(derived.specs(Some(&["nope".to_string()])).is_empty());

        // Unregister removes it everywhere.
        assert!(base.unregister_dynamic("echo").is_some());
        assert!(!derived.contains("echo"));
        assert!(derived.specs(None).iter().all(|s| s.name != "echo"));
    }

    #[test]
    fn static_tool_shadows_a_same_named_overlay_tool() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(EchoTool));
        reg.register_dynamic(Arc::new(EchoTool));
        // No duplicate spec; the static one wins.
        assert_eq!(
            reg.specs(None).iter().filter(|s| s.name == "echo").count(),
            1
        );
    }

    /// `dispatch` injects the registry it ran on into the invoked tool's context —
    /// the seam a nested-call host (`run_javascript`'s `catalerum.callTool`)
    /// re-dispatches through — while a caller-supplied context never carries one
    /// (a direct `invoke` keeps `registry: None`, so a nested host fails closed).
    #[test]
    fn dispatch_injects_its_registry_into_the_invoke_context() {
        struct RegistryProbe;
        #[async_trait]
        impl Tool for RegistryProbe {
            fn name(&self) -> &str {
                "probe"
            }
            fn parameters_schema(&self) -> Json {
                serde_json::json!({ "type": "object" })
            }
            async fn invoke(&self, _args: Json, ctx: &ToolContext) -> Result<Json> {
                // The injected registry is the dispatching one: it can see this
                // very tool.
                let sees_self = ctx.registry.as_ref().is_some_and(|r| r.contains("probe"));
                Ok(serde_json::json!({ "injected": sees_self }))
            }
        }
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(RegistryProbe));
        futures::executor::block_on(async {
            let ctx = ToolContext::default();
            assert!(ctx.registry.is_none(), "callers never pre-set the registry");
            let out = reg
                .dispatch("probe", serde_json::json!({}), &ctx)
                .await
                .unwrap();
            assert_eq!(out, serde_json::json!({ "injected": true }));
        });
    }

    #[test]
    fn specs_for_looks_up_exact_names_in_order_skipping_unknown() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(EchoTool));
        reg.register_dynamic(Arc::new(GatedTool));
        let specs = reg.specs_for(&[
            "gated".to_string(),
            "missing".to_string(),
            "echo".to_string(),
        ]);
        let names: Vec<_> = specs.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["gated", "echo"],
            "caller order kept, unknown skipped"
        );
    }

    #[test]
    fn domain_groups_sorts_by_capability_domain_with_ungated_as_general() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(EchoTool)); // ungated → "general"
        reg.register(Arc::new(GatedTool)); // notes:write → "notes"
        let groups = reg.domain_groups(None);
        assert_eq!(
            groups,
            vec![
                ("general".to_string(), vec!["echo".to_string()]),
                ("notes".to_string(), vec!["gated".to_string()]),
            ]
        );
        // The `allowed` filter applies here too.
        let groups = reg.domain_groups(Some(&["gated".to_string()]));
        assert_eq!(
            groups,
            vec![("notes".to_string(), vec!["gated".to_string()])]
        );
    }

    #[test]
    fn dispatch_enforces_required_capability_deny_by_default() {
        use crate::capability::{Action, Capability, Resource};
        futures::executor::block_on(async {
            let mut reg = ToolRegistry::new();
            reg.register(Arc::new(GatedTool));
            let args = serde_json::json!({});

            // No capabilities supplied → enforcement is off (legacy/internal caller).
            assert!(reg
                .dispatch("gated", args.clone(), &ToolContext::default())
                .await
                .is_ok());

            // Caps that DON'T cover notes:write → denied.
            let read_only = ToolContext {
                capabilities: Some(vec![Capability::new(
                    Action::Read,
                    Resource::domain("notes"),
                )]),
                ..Default::default()
            };
            let err = reg
                .dispatch("gated", args.clone(), &read_only)
                .await
                .unwrap_err();
            assert!(
                matches!(err, Error::Unauthorized(_)),
                "read-only must be denied, got {err:?}"
            );

            // A covering capability → allowed.
            let writer = ToolContext {
                capabilities: Some(vec![Capability::new(
                    Action::Write,
                    Resource::domain("notes"),
                )]),
                ..Default::default()
            };
            assert!(reg.dispatch("gated", args.clone(), &writer).await.is_ok());

            // The wildcard (owner) capability covers everything.
            let owner = ToolContext {
                capabilities: Some(vec![Capability::new(Action::Any, Resource::any())]),
                ..Default::default()
            };
            assert!(reg.dispatch("gated", args, &owner).await.is_ok());
        });
    }

    /// A tool that records whether it was actually invoked — proves `dry_run`
    /// short-circuits *before* `invoke`, so the side effect never fires.
    struct SpyTool(Arc<std::sync::atomic::AtomicBool>);

    #[async_trait]
    impl Tool for SpyTool {
        fn name(&self) -> &str {
            "spy"
        }
        fn parameters_schema(&self) -> Json {
            serde_json::json!({ "type": "object" })
        }
        fn required_capability(&self) -> Option<Capability> {
            Some(Capability::new(
                crate::capability::Action::Write,
                crate::capability::Resource::domain("notes"),
            ))
        }
        async fn invoke(&self, _args: Json, _ctx: &ToolContext) -> Result<Json> {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(serde_json::json!({ "committed": true }))
        }
    }

    #[test]
    fn dispatch_dry_run_simulates_without_invoking_but_still_enforces_caps() {
        use crate::capability::{Action, Capability, Resource};
        use std::sync::atomic::{AtomicBool, Ordering};
        futures::executor::block_on(async {
            let invoked = Arc::new(AtomicBool::new(false));
            let mut reg = ToolRegistry::new();
            reg.register(Arc::new(SpyTool(invoked.clone())));
            let args = serde_json::json!({ "x": 1 });

            // dry_run + covering caps → simulated marker, `invoke` NEVER runs.
            let dry_writer = ToolContext {
                capabilities: Some(vec![Capability::new(
                    Action::Write,
                    Resource::domain("notes"),
                )]),
                dry_run: true,
                ..Default::default()
            };
            let out = reg
                .dispatch("spy", args.clone(), &dry_writer)
                .await
                .unwrap();
            assert_eq!(out["dry_run"], serde_json::json!(true));
            assert_eq!(out["tool"], serde_json::json!("spy"));
            assert_eq!(out["args"], args);
            assert!(
                !invoked.load(Ordering::SeqCst),
                "dry_run must short-circuit before invoke — no side effect"
            );

            // dry_run does NOT bypass the capability gate: an unauthorized dry_run is
            // still denied (you can't even *simulate* a tool you can't call).
            let dry_reader = ToolContext {
                capabilities: Some(vec![Capability::new(
                    Action::Read,
                    Resource::domain("notes"),
                )]),
                dry_run: true,
                ..Default::default()
            };
            let err = reg.dispatch("spy", args, &dry_reader).await.unwrap_err();
            assert!(
                matches!(err, Error::Unauthorized(_)),
                "dry_run still deny-by-default, got {err:?}"
            );
            assert!(!invoked.load(Ordering::SeqCst));
        });
    }

    /// A gate that denies at a chosen phase (with a reason) and allows otherwise —
    /// enough to prove the two `dispatch` deny paths.
    struct PhaseGate(GatePhase);

    #[async_trait]
    impl ToolGate for PhaseGate {
        async fn review(
            &self,
            phase: GatePhase,
            _tool: &dyn Tool,
            _args: &Json,
            _output: Option<&Json>,
            _ctx: &ToolContext,
        ) -> GateVerdict {
            if phase == self.0 {
                GateVerdict::Deny {
                    reason: "blocked by test guard".to_string(),
                }
            } else {
                GateVerdict::Allow
            }
        }
    }

    #[test]
    fn gate_call_phase_deny_short_circuits_before_invoke() {
        use std::sync::atomic::{AtomicBool, Ordering};
        futures::executor::block_on(async {
            let invoked = Arc::new(AtomicBool::new(false));
            let mut reg = ToolRegistry::new();
            reg.register(Arc::new(SpyTool(invoked.clone())));

            let ctx = ToolContext {
                // Cover the tool's capability so we're testing the *gate*, not caps.
                capabilities: Some(vec![Capability::new(
                    crate::capability::Action::Write,
                    crate::capability::Resource::domain("notes"),
                )]),
                gate: Some(Arc::new(PhaseGate(GatePhase::Call))),
                ..Default::default()
            };
            let err = reg
                .dispatch("spy", serde_json::json!({}), &ctx)
                .await
                .unwrap_err();
            assert!(
                matches!(err, Error::Denied(_)),
                "call-phase deny must be Error::Denied, got {err:?}"
            );
            assert!(
                !invoked.load(Ordering::SeqCst),
                "a call-phase deny must short-circuit before the side effect"
            );
        });
    }

    #[test]
    fn gate_output_phase_deny_withholds_the_result_after_invoke() {
        use std::sync::atomic::{AtomicBool, Ordering};
        futures::executor::block_on(async {
            let invoked = Arc::new(AtomicBool::new(false));
            let mut reg = ToolRegistry::new();
            reg.register(Arc::new(SpyTool(invoked.clone())));

            let ctx = ToolContext {
                capabilities: Some(vec![Capability::new(
                    crate::capability::Action::Write,
                    crate::capability::Resource::domain("notes"),
                )]),
                gate: Some(Arc::new(PhaseGate(GatePhase::Output))),
                ..Default::default()
            };
            let out = reg
                .dispatch("spy", serde_json::json!({}), &ctx)
                .await
                .expect("output-phase deny returns a marker, not an error");
            // The side effect DID run (deny can't unrun it) but the result is withheld.
            assert!(invoked.load(Ordering::SeqCst));
            assert_eq!(out["policy_withheld"], serde_json::json!(true));
            assert!(out["reason"].as_str().unwrap().contains("test guard"));
        });
    }

    #[test]
    fn gate_allow_lets_the_call_through_unchanged() {
        // A gate whose every phase allows must be transparent — dispatch returns the
        // tool's real output.
        struct AllowGate;
        #[async_trait]
        impl ToolGate for AllowGate {
            async fn review(
                &self,
                _p: GatePhase,
                _t: &dyn Tool,
                _a: &Json,
                _o: Option<&Json>,
                _c: &ToolContext,
            ) -> GateVerdict {
                GateVerdict::Allow
            }
        }
        futures::executor::block_on(async {
            let mut reg = ToolRegistry::new();
            reg.register(Arc::new(EchoTool));
            let ctx = ToolContext {
                gate: Some(Arc::new(AllowGate)),
                ..Default::default()
            };
            let out = reg
                .dispatch("echo", serde_json::json!({ "x": 1 }), &ctx)
                .await
                .unwrap();
            assert_eq!(out, serde_json::json!({ "x": 1 }));
        });
    }
}
