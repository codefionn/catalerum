//! The runtime behind a profile's **tool guard** (SOUL §19): a programmable
//! classifier — Boa JS and/or an LLM judge — consulted for every tool call a
//! guarded profile makes, layered *on top of* the deny-by-default capability
//! check at [`ToolRegistry::dispatch`].
//!
//! A [`ToolGuard`](catalerum_core::model::ToolGuard) config is turned into a
//! [`ProfileToolGate`] (an implementation of the core [`ToolGate`] trait). For
//! each tool call the gate builds a JSON description of the call, runs the
//! classifier, and maps its `allow` / `deny` / `require-user-feedback` decision to
//! a [`GateVerdict`]. A "require-user-feedback" decision blocks on an [`Approver`]
//! (the interactive chat supplies one; a headless run does not → deny).
//!
//! The guard can only ever **tighten** authority: it runs after the capability
//! gate, and its own re-entrant tool lookups (`catalerum.callTool`) dispatch under
//! a context with the gate cleared, so a classifier is never re-classified.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use catalerum_core::capability::Action;
use catalerum_core::llm::{ChatMessage, ChatRequest};
use catalerum_core::model::{GuardFail, ToolGuard};
use catalerum_core::tool::{GatePhase, GateVerdict, Tool, ToolContext, ToolGate, ToolRegistry};
use catalerum_llm::OpenRouterClient;
use catalerum_script::{ScriptCodeRunner, UiScriptHost};
use catalerum_store::Store;

/// The default judge system prompt used when a declarative LLM guard leaves the
/// instruction blank (it normally supplies its own).
const DEFAULT_JUDGE_SYSTEM: &str =
    "You are a security classifier deciding whether an AI agent may run a tool call. \
     Reply with a compact JSON object {\"decision\": \"allow\"|\"deny\"|\"ask\", \"reason\": \"…\"}. \
     Use \"ask\" when a human should confirm.";

// ---------------------------------------------------------------------------
// Durable, restart-proof approval (SOUL §7/§12/§19)
// ---------------------------------------------------------------------------
//
// A "require-user-feedback" outcome does NOT block the turn. The gate records a
// durable [`PendingApproval`](catalerum_core::model::PendingApproval) tied to the
// conversation and **defers** the call (the tool is held, the turn ends). The
// client renders an Approve/Reject prompt — pushed live, and re-fetchable on
// reload/reconnect/restart. On Approve the agent re-runs the call and the gate,
// finding the recorded decision, allows it; on Reject it denies with the reason. A
// run with no conversation (automation / channel worker / delegated subagent) has
// no one to ask, so an `ask` there fails closed.

/// Build a [`ToolGate`] from a profile's optional `guard`, or `None` when the
/// profile has no guard (leaving it gated only by its capabilities).
///
/// `base_ctx` is the run's dispatch context — its gate is cleared here so the
/// classifier's own `catalerum.callTool` lookups aren't re-classified.
/// `fallback_model` is the profile's model (or the workspace default); the guard's
/// `llm.model`, when set, overrides it for the judge.
#[must_use]
pub fn build_gate(
    guard: Option<&ToolGuard>,
    registry: ToolRegistry,
    store: Store,
    base_ctx: ToolContext,
    llm: OpenRouterClient,
    fallback_model: String,
) -> Option<Arc<dyn ToolGate>> {
    build_gate_with_context(guard, registry, store, base_ctx, llm, fallback_model, None)
}

/// Build the same profile-compatible Boa/LLM gate while binding an additional
/// immutable `input.policy_context` value into every call/output review. This is
/// used by constrained subagents: the policy can compare attempted tool
/// arguments with the exact PR / user-story identifiers selected by the parent
/// without exposing that context as child-controlled tool arguments.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn build_gate_with_context(
    guard: Option<&ToolGuard>,
    registry: ToolRegistry,
    store: Store,
    mut base_ctx: ToolContext,
    llm: OpenRouterClient,
    fallback_model: String,
    policy_context: Option<Value>,
) -> Option<Arc<dyn ToolGate>> {
    let guard = guard?.clone();
    let model = guard
        .llm
        .as_ref()
        .and_then(|l| l.model.clone())
        .unwrap_or(fallback_model);
    base_ctx.gate = None; // never let the classifier's own lookups be re-gated
    Some(Arc::new(ProfileToolGate::new(
        guard,
        registry,
        store,
        base_ctx,
        llm,
        model,
        policy_context,
    )) as Arc<dyn ToolGate>)
}

/// Combine tightening gates. Every gate must allow an event; the first deny or
/// deferral wins. This lets a constrained launcher retain its mandatory boundary
/// policy while also honoring a selected agent profile's guard.
#[must_use]
pub fn all_gates(
    gates: impl IntoIterator<Item = Option<Arc<dyn ToolGate>>>,
) -> Option<Arc<dyn ToolGate>> {
    let gates = gates.into_iter().flatten().collect::<Vec<_>>();
    match gates.len() {
        0 => None,
        1 => gates.into_iter().next(),
        _ => Some(Arc::new(AllToolGates { gates })),
    }
}

struct AllToolGates {
    gates: Vec<Arc<dyn ToolGate>>,
}

#[async_trait]
impl ToolGate for AllToolGates {
    async fn review(
        &self,
        phase: GatePhase,
        tool: &dyn Tool,
        args: &Value,
        output: Option<&Value>,
        ctx: &ToolContext,
    ) -> GateVerdict {
        for gate in &self.gates {
            let verdict = gate.review(phase, tool, args, output, ctx).await;
            if !matches!(verdict, GateVerdict::Allow) {
                return verdict;
            }
        }
        GateVerdict::Allow
    }
}

/// A classifier decision, with a reason for the non-allow outcomes.
enum Classification {
    Allow,
    Deny(String),
    Ask(String),
}

/// A stored object a tool call references, with its labels resolved (SOUL §9).
struct ResolvedObject {
    store: String,
    path: String,
    labels: Vec<String>,
}

/// The [`ToolGate`] a guarded profile runs its tool calls through. Built per run
/// (per chat turn / per profile invocation) from the profile's [`ToolGuard`].
pub struct ProfileToolGate {
    guard: ToolGuard,
    runner: Arc<ScriptCodeRunner>,
    host: Arc<ClassifierHost>,
    /// Resolves object labels for a call (the object-label policy + the classifier
    /// `input`), and holds the durable [`PendingApproval`] records for the
    /// require-user-feedback path.
    ///
    /// [`PendingApproval`]: catalerum_core::model::PendingApproval
    store: Store,
    /// Immutable parent-selected data bound into each classifier input. It is
    /// never sourced from the attempted tool call, so a child cannot forge the
    /// PR/story identifiers its policy compares against.
    policy_context: Option<Value>,
}

impl ProfileToolGate {
    /// Build a gate from a profile's `guard` config.
    ///
    /// `base_ctx` is the profile's own dispatch context **with the gate cleared**
    /// (`gate: None`) — the authority the classifier's `catalerum.callTool`
    /// lookups run under (so they can't re-trigger classification). `default_model`
    /// is the model the LLM judge / `classifyWithLlm` uses when the config doesn't
    /// override it.
    #[must_use]
    pub fn new(
        guard: ToolGuard,
        registry: ToolRegistry,
        store: Store,
        base_ctx: ToolContext,
        llm: OpenRouterClient,
        default_model: String,
        policy_context: Option<Value>,
    ) -> Self {
        let default_instruction = guard.llm.as_ref().map(|l| l.instruction.clone());
        let host = Arc::new(ClassifierHost {
            registry,
            base_ctx,
            llm,
            default_model,
            default_instruction,
        });
        Self {
            guard,
            runner: Arc::new(ScriptCodeRunner::new()),
            host,
            store,
            policy_context,
        }
    }

    /// Resolve the labels of the object a call references, if any (SOUL §9). Looks
    /// for a `key`/`path` string arg (the object path) + an optional `store`
    /// selector, and reads the labels on that `(store, path)`. `None` when the call
    /// references no object, has no workspace, or the lookup fails.
    async fn resolve_object(&self, args: &Value, ctx: &ToolContext) -> Option<ResolvedObject> {
        let ws = ctx.workspace_id?;
        let raw = args
            .get("key")
            .and_then(Value::as_str)
            .or_else(|| args.get("path").and_then(Value::as_str))?;
        let path = raw.trim().trim_end_matches('/');
        if path.is_empty() {
            return None;
        }
        let store = args
            .get("store")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let labels = match self.store.object_labels().list_for(ws, &store, path).await {
            Ok(rows) => rows.into_iter().map(|l| l.label).collect(),
            Err(e) => {
                tracing::warn!(error = %e, "resolving object labels for tool guard");
                return None;
            }
        };
        Some(ResolvedObject {
            store,
            path: path.to_string(),
            labels,
        })
    }

    /// Build the JSON description of a call the classifier sees as `input`.
    fn build_input(
        &self,
        phase: GatePhase,
        tool: &dyn Tool,
        args: &Value,
        output: Option<&Value>,
        ctx: &ToolContext,
        object: Option<&ResolvedObject>,
    ) -> Value {
        let cap = tool.required_capability();
        let capability = cap.as_ref().map(|c| {
            json!({
                "domain": c.resource.domain,
                "selector": c.resource.selector,
                "action": action_str(c.action),
                "read_only": c.action == Action::Read,
            })
        });
        // MCP tools are gated on `mcp:use@{server}` — surface the server so a guard
        // can say "this MCP server is read-only".
        let mcp = cap
            .as_ref()
            .filter(|c| c.resource.domain == "mcp")
            .map(|c| json!({ "server": c.resource.selector }));

        // The object (file/dir) this call touches + its labels, when any (SOUL §9),
        // so a classifier can decide by label.
        let object =
            object.map(|o| json!({ "store": o.store, "path": o.path, "labels": o.labels }));

        json!({
            "phase": match phase { GatePhase::Call => "call", GatePhase::Output => "output" },
            "tool": { "name": tool.name(), "description": tool.description() },
            "capability": capability,
            "mcp": mcp,
            "args": args,
            "output": output,
            "object": object,
            "workspace_id": ctx.workspace_id.map(|w| w.to_string()),
            "user_id": ctx.user_id.map(|u| u.to_string()),
            "agent_id": ctx.agent_id.map(|a| a.to_string()),
            "policy_context": self.policy_context,
        })
    }

    /// Run the configured classifier and produce a decision. `None` means the
    /// classifier failed or was unrecognized — the caller applies `on_error`.
    async fn classify(&self, input: &Value) -> Option<Classification> {
        if let Some(script) = &self.guard.script {
            match self
                .runner
                .run_guard(script, input, self.host.clone() as Arc<dyn UiScriptHost>)
                .await
            {
                Ok(v) => parse_classification(&v),
                Err(e) => {
                    tracing::warn!(error = %e, "tool-guard script failed");
                    None
                }
            }
        } else if self.guard.llm.is_some() {
            // Declarative LLM guard: judge the described call directly. The judge
            // uses the config's instruction (via the host's default) as its system
            // prompt and the call description as the user turn.
            match self.host.judge(input.clone()).await {
                Ok(v) => parse_classification(&v),
                Err(e) => {
                    tracing::warn!(error = %e, "tool-guard llm judge failed");
                    None
                }
            }
        } else {
            // No classifier configured → the guard is inert.
            Some(Classification::Allow)
        }
    }

    /// The decision to use when the classifier can't produce one (its `on_error`).
    fn on_error_decision(&self) -> Classification {
        match self.guard.on_error {
            GuardFail::Deny => {
                Classification::Deny("tool guard could not classify the call".into())
            }
            GuardFail::Allow => Classification::Allow,
            GuardFail::Ask => Classification::Ask("tool guard could not classify the call".into()),
        }
    }

    /// Resolve a classifier decision to a verdict. An `ask` is handled durably (see
    /// [`defer_for_approval`](Self::defer_for_approval)).
    async fn resolve(
        &self,
        decision: Classification,
        tool: &dyn Tool,
        args: &Value,
        ctx: &ToolContext,
    ) -> GateVerdict {
        match decision {
            Classification::Allow => GateVerdict::Allow,
            Classification::Deny(reason) => GateVerdict::Deny { reason },
            Classification::Ask(reason) => self.defer_for_approval(tool, args, ctx, reason).await,
        }
    }

    /// The require-user-feedback path (SOUL §19), durable + restart-proof.
    ///
    /// First checks for a decision the user already made on this exact re-attempted
    /// call (Approve → [`Allow`](GateVerdict::Allow), Reject → [`Deny`]). Otherwise
    /// records a durable [`PendingApproval`](catalerum_core::model::PendingApproval)
    /// (reusing an existing unresolved one for this call) and **defers** — the tool
    /// is held and the model reads an `awaiting_approval` marker. A run with no
    /// conversation (automation / channel worker / delegated subagent) has no one to
    /// ask, so it fails closed.
    ///
    /// [`Deny`]: GateVerdict::Deny
    async fn defer_for_approval(
        &self,
        tool: &dyn Tool,
        args: &Value,
        ctx: &ToolContext,
        reason: String,
    ) -> GateVerdict {
        use catalerum_core::model::ApprovalDecision;
        let (Some(ws), Some(conv)) = (ctx.workspace_id, ctx.conversation_id) else {
            return GateVerdict::Deny {
                reason: format!(
                    "requires your approval, but no interactive conversation is available here ({reason})"
                ),
            };
        };
        let pending = self.store.pending_approvals();

        // Resume: did the user already decide this exact call? (Consumes the record.)
        match pending.take_resolved(ws, conv, tool.name(), args).await {
            Ok(Some(ApprovalDecision::Approved)) => return GateVerdict::Allow,
            Ok(Some(ApprovalDecision::Rejected)) => {
                return GateVerdict::Deny {
                    reason: format!("you rejected this tool call ({reason})"),
                };
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(error = %e, "tool-guard: reading approval decision"),
        }

        // Defer: reuse an unresolved record for THIS call, else record a new one.
        // At most one call is held per conversation — a second, different guarded
        // call while one is pending is denied (the model should end its turn).
        let id = match pending.get_unresolved(ws, conv).await {
            Ok(Some(existing)) if existing.tool == tool.name() && existing.arguments == *args => {
                existing.id
            }
            Ok(Some(_)) => {
                return GateVerdict::Deny {
                    reason: "another tool call is already awaiting the user's approval; \
                             end your turn"
                        .into(),
                };
            }
            _ => match pending.create(ws, conv, tool.name(), args, &reason).await {
                Ok(p) => p.id,
                Err(e) => {
                    tracing::error!(error = %e, "tool-guard: recording pending approval");
                    // Fail closed — can't record it, so don't run it unapproved.
                    return GateVerdict::Deny {
                        reason: format!("could not record the approval request ({reason})"),
                    };
                }
            },
        };

        GateVerdict::Defer {
            result: json!({
                "status": "awaiting_approval",
                "pending_approval_id": id.to_string(),
                "tool": tool.name(),
                "arguments": args,
                "reason": reason,
                "note": "This tool call requires the user's approval and has been queued. \
                         STOP and end your turn now — do not call more tools or answer on the \
                         user's behalf. It will run once the user approves.",
            }),
        }
    }
}

#[async_trait]
impl ToolGate for ProfileToolGate {
    async fn review(
        &self,
        phase: GatePhase,
        tool: &dyn Tool,
        args: &Value,
        output: Option<&Value>,
        ctx: &ToolContext,
    ) -> GateVerdict {
        // Resolve the labels of any object this call touches (SOUL §9) — needed for
        // both the declarative policy and the classifier `input`.
        let object = self.resolve_object(args, ctx).await;

        // Declarative object-label policy: a hard allow/deny by label, applied
        // before the classifier and only on the call phase (the object exists then).
        if phase == GatePhase::Call {
            if let (Some(policy), Some(obj)) = (&self.guard.object_labels, &object) {
                if let Some(reason) = policy.violation(&obj.labels) {
                    return GateVerdict::Deny { reason };
                }
            }
        }

        let input = self.build_input(phase, tool, args, output, ctx, object.as_ref());
        let decision = self
            .classify(&input)
            .await
            .unwrap_or_else(|| self.on_error_decision());
        self.resolve(decision, tool, args, ctx).await
    }
}

// ---------------------------------------------------------------------------
// The classifier's host bridge
// ---------------------------------------------------------------------------

/// The [`UiScriptHost`] a guard script reaches the server through: `callTool`
/// dispatches under the profile's authority (with the gate cleared, so a lookup
/// isn't itself gated) and `classifyWithLlm` runs the LLM judge.
struct ClassifierHost {
    registry: ToolRegistry,
    /// The profile's context with `gate: None` — authority for re-entrant lookups.
    base_ctx: ToolContext,
    llm: OpenRouterClient,
    default_model: String,
    default_instruction: Option<String>,
}

impl ClassifierHost {
    /// Ask the LLM to classify. `req` is a free-form object: `model` /
    /// `instruction` override the defaults, `messages` (an array of `{role,
    /// content}`) overrides the auto-built prompt; otherwise the whole `req` is
    /// rendered as the described call. Returns `{ decision, reason, text }`.
    async fn judge(&self, req: Value) -> Result<Value, String> {
        let model = req
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| self.default_model.clone());
        let system = req
            .get("instruction")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| self.default_instruction.clone())
            .unwrap_or_else(|| DEFAULT_JUDGE_SYSTEM.to_string());

        let mut messages = vec![ChatMessage::system(system)];
        if let Some(arr) = req.get("messages").and_then(Value::as_array) {
            for m in arr {
                let content = m.get("content").and_then(Value::as_str).unwrap_or_default();
                match m.get("role").and_then(Value::as_str) {
                    Some("system") => messages.push(ChatMessage::system(content)),
                    Some("assistant") => messages.push(ChatMessage::assistant(content)),
                    _ => messages.push(ChatMessage::user(content)),
                }
            }
        } else {
            let described = serde_json::to_string_pretty(&req).unwrap_or_else(|_| req.to_string());
            messages.push(ChatMessage::user(format!(
                "Classify this tool call as \"allow\", \"deny\", or \"ask\" (require user \
                 feedback). Reply with JSON {{\"decision\": \"…\", \"reason\": \"…\"}}.\n\n{described}"
            )));
        }

        let turn = self
            .llm
            .chat(ChatRequest::new(model, messages))
            .await
            .map_err(|e| e.to_string())?;
        let (decision, reason) = parse_decision(&turn.content);
        Ok(json!({ "decision": decision, "reason": reason, "text": turn.content }))
    }
}

impl UiScriptHost for ClassifierHost {
    fn call_tool(&self, tool: &str, args: Value) -> Result<Value, String> {
        if !self.registry.contains(tool) {
            return Err(format!("unknown tool `{tool}`"));
        }
        // Synchronous on the script's `spawn_blocking` thread → `block_on` is valid.
        let handle = tokio::runtime::Handle::current();
        handle
            .block_on(self.registry.dispatch(tool, args, &self.base_ctx))
            .map_err(|e| e.to_string())
    }

    fn classify_llm(&self, req: Value) -> Result<Value, String> {
        let handle = tokio::runtime::Handle::current();
        handle.block_on(self.judge(req))
    }
}

// ---------------------------------------------------------------------------
// Decision parsing (lenient)
// ---------------------------------------------------------------------------

/// Normalize a free-form decision word to `allow` / `deny` / `ask`, or `unknown`.
fn normalize_decision(raw: &str) -> &'static str {
    let s = raw.trim().to_ascii_lowercase();
    // Prefer an exact token, then a contained keyword (tolerates a sentence).
    let has = |needle: &str| s == needle || s.contains(needle);
    if has("require-user-feedback")
        || has("require_user_feedback")
        || has("feedback")
        || has("ask")
        || has("confirm")
        || has("approve")
    {
        "ask"
    } else if has("deny") || has("block") || has("reject") || has("refuse") || has("forbid") {
        "deny"
    } else if has("allow") || has("permit") || s == "ok" || has("approved") {
        "allow"
    } else {
        "unknown"
    }
}

/// Turn a classifier return value (a string decision, or an object with a
/// `decision`/`verdict` field + optional `reason`) into a [`Classification`].
/// `None` for anything unrecognized (→ the caller's `on_error`).
fn parse_classification(v: &Value) -> Option<Classification> {
    let (decision, reason) = match v {
        Value::String(s) => (normalize_decision(s).to_string(), String::new()),
        Value::Object(_) => {
            let decision = v
                .get("decision")
                .or_else(|| v.get("verdict"))
                .and_then(Value::as_str)
                .map(normalize_decision)
                .unwrap_or("unknown")
                .to_string();
            let reason = v
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            (decision, reason)
        }
        _ => return None,
    };
    classification_from(&decision, reason)
}

fn classification_from(decision: &str, reason: String) -> Option<Classification> {
    let with = |fallback: &str| {
        if reason.trim().is_empty() {
            fallback.to_string()
        } else {
            reason.clone()
        }
    };
    match decision {
        "allow" => Some(Classification::Allow),
        "deny" => Some(Classification::Deny(with(
            "blocked by the profile's tool guard",
        ))),
        "ask" => Some(Classification::Ask(with(
            "the profile's tool guard requires your approval",
        ))),
        _ => None,
    }
}

/// Parse an LLM reply into `(decision, reason)`. Prefers a JSON object anywhere in
/// the text; falls back to a keyword scan.
fn parse_decision(text: &str) -> (String, String) {
    if let Some(obj) = extract_json_object(text) {
        let decision = obj
            .get("decision")
            .or_else(|| obj.get("verdict"))
            .and_then(Value::as_str)
            .map(normalize_decision)
            .unwrap_or("unknown");
        if decision != "unknown" {
            let reason = obj
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            return (decision.to_string(), reason);
        }
    }
    (normalize_decision(text).to_string(), String::new())
}

/// Extract the first balanced `{ … }` span from `text` and parse it as JSON.
fn extract_json_object(text: &str) -> Option<Value> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end > start {
        serde_json::from_str(&text[start..=end]).ok()
    } else {
        None
    }
}

/// The lowercase token for an [`Action`], mirroring the core error formatter.
fn action_str(action: Action) -> &'static str {
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

#[cfg(test)]
mod tests {
    use super::*;
    use catalerum_core::capability::{Capability, Resource};
    use serde_json::json;

    /// A minimal tool that declares a chosen capability — enough for the gate to
    /// build its `input` and for MCP/read-only detection.
    struct StubTool {
        name: &'static str,
        cap: Option<Capability>,
    }

    #[async_trait]
    impl Tool for StubTool {
        fn name(&self) -> &str {
            self.name
        }
        fn required_capability(&self) -> Option<Capability> {
            self.cap.clone()
        }
        fn parameters_schema(&self) -> Value {
            json!({ "type": "object" })
        }
        async fn invoke(&self, args: Value, _ctx: &ToolContext) -> catalerum_core::Result<Value> {
            Ok(args)
        }
    }

    /// A no-connect [`Store`] for the script-path tests: a lazy pool that never
    /// dials Postgres (these tests reference no object and no conversation, so
    /// neither `resolve_object` nor the approval repo ever queries it).
    fn lazy_store() -> Store {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost/catalerum_test")
            .expect("lazy pool");
        Store::new(pool)
    }

    /// A gate with a script guard and no LLM — the LLM client is a stub that is
    /// never called (the scripts here don't invoke `classifyWithLlm`).
    fn script_gate(script: &str, on_error: GuardFail) -> ProfileToolGate {
        ProfileToolGate::new(
            ToolGuard {
                script: Some(script.to_string()),
                llm: None,
                object_labels: None,
                on_error,
            },
            ToolRegistry::new(),
            lazy_store(),
            ToolContext::default(),
            OpenRouterClient::new("http://127.0.0.1:1", "test-key"),
            "test-model".to_string(),
            None,
        )
    }

    /// Review with the default (no-conversation) context — so an `ask` fails closed
    /// and no approval repo query fires against the lazy pool.
    async fn verdict_for(gate: &ProfileToolGate, tool: &dyn Tool, args: Value) -> GateVerdict {
        gate.review(GatePhase::Call, tool, &args, None, &ToolContext::default())
            .await
    }

    #[tokio::test]
    async fn script_deny_and_allow() {
        let tool = StubTool {
            name: "delete_object",
            cap: None,
        };
        let deny = script_gate(
            "return input.tool.name === 'delete_object' ? 'deny' : 'allow';",
            GuardFail::Deny,
        );
        assert!(matches!(
            verdict_for(&deny, &tool, json!({})).await,
            GateVerdict::Deny { .. }
        ));
        let allow = script_gate("return 'allow';", GuardFail::Deny);
        assert!(matches!(
            verdict_for(&allow, &tool, json!({})).await,
            GateVerdict::Allow
        ));
    }

    #[tokio::test]
    async fn combined_gates_require_every_policy_to_allow() {
        let tool = StubTool {
            name: "terminal_write",
            cap: None,
        };
        let allow: Arc<dyn ToolGate> = Arc::new(script_gate("return 'allow';", GuardFail::Deny));
        let deny: Arc<dyn ToolGate> = Arc::new(script_gate(
            "return {decision:'deny', reason:'profile boundary'};",
            GuardFail::Deny,
        ));
        let combined = all_gates([Some(allow), Some(deny)]).expect("combined gate");

        let verdict = combined
            .review(
                GatePhase::Call,
                &tool,
                &json!({ "data": "cargo test" }),
                None,
                &ToolContext::default(),
            )
            .await;
        assert!(matches!(verdict, GateVerdict::Deny { reason } if reason == "profile boundary"));
    }

    #[tokio::test]
    async fn script_can_compare_tool_arguments_with_immutable_policy_context() {
        let gate = ProfileToolGate::new(
            ToolGuard {
                script: Some(
                    "if (input.phase !== 'call') return 'allow';\n\
                     var expected = input.policy_context.pull_request.id;\n\
                     var attempted = input.args.arguments.pull_request_id;\n\
                     return attempted === expected ? 'allow' : {decision:'deny', reason:'wrong PR'};"
                        .into(),
                ),
                llm: None,
                object_labels: None,
                on_error: GuardFail::Deny,
            },
            ToolRegistry::new(),
            lazy_store(),
            ToolContext::default(),
            OpenRouterClient::new("http://127.0.0.1:1", "test-key"),
            "test-model".into(),
            Some(json!({ "pull_request": { "id": "pr-42" } })),
        );
        let upstream = StubTool {
            name: "upstream",
            cap: None,
        };
        assert!(matches!(
            verdict_for(
                &gate,
                &upstream,
                json!({ "tool": "update_pr", "arguments": { "pull_request_id": "pr-42" } })
            )
            .await,
            GateVerdict::Allow
        ));
        assert!(matches!(
            verdict_for(
                &gate,
                &upstream,
                json!({ "tool": "update_pr", "arguments": { "pull_request_id": "pr-99" } })
            )
            .await,
            GateVerdict::Deny { .. }
        ));
    }

    #[tokio::test]
    async fn ask_without_a_conversation_denies() {
        // A run with no interactive conversation (automation / channel / subagent)
        // has no one to ask, so an `ask` fails closed — no repo query needed.
        let tool = StubTool {
            name: "write_object",
            cap: None,
        };
        let gate = script_gate("return 'ask';", GuardFail::Deny);
        assert!(matches!(
            verdict_for(&gate, &tool, json!({})).await,
            GateVerdict::Deny { .. }
        ));
    }

    /// The durable approval flow end-to-end against a real store (DB-gated,
    /// self-skips): an `ask` DEFERS + records a pending approval; approving it makes
    /// the re-attempt ALLOW; a fresh `ask` after a reject DENIES.
    #[tokio::test]
    async fn ask_defers_then_resumes_from_the_durable_record() {
        let Some(url) = std::env::var("CATALERUM_TEST_DATABASE_URL")
            .or_else(|_| std::env::var("DATABASE_URL"))
            .ok()
        else {
            eprintln!("skipping ask_defers_then_resumes_…: set CATALERUM_TEST_DATABASE_URL");
            return;
        };
        use catalerum_core::model::{ApprovalDecision, Origin};
        let store = Store::connect(&url).await.expect("connect+migrate");
        let ws = store
            .workspaces()
            .create("ga", &format!("ga-{}", uuid::Uuid::new_v4()))
            .await
            .expect("ws");
        let conv = store
            .conversations()
            .create(ws.id, Some("t"), Origin::Web)
            .await
            .expect("conv");

        let gate = ProfileToolGate::new(
            ToolGuard {
                script: Some("return 'ask';".into()),
                llm: None,
                object_labels: None,
                on_error: GuardFail::Deny,
            },
            ToolRegistry::new(),
            store.clone(),
            ToolContext::default(),
            OpenRouterClient::new("http://127.0.0.1:1", "k"),
            "m".into(),
            None,
        );
        let tool = StubTool {
            name: "write_object",
            cap: None,
        };
        let ctx = ToolContext {
            workspace_id: Some(ws.id),
            conversation_id: Some(conv.id),
            ..Default::default()
        };
        let args = json!({ "key": "reports/q1.pdf" });

        // First encounter → Defer + a durable record exists.
        let v = gate.review(GatePhase::Call, &tool, &args, None, &ctx).await;
        assert!(matches!(v, GateVerdict::Defer { .. }));
        let pending = store
            .pending_approvals()
            .get_unresolved(ws.id, conv.id)
            .await
            .unwrap()
            .expect("a pending approval was recorded");
        assert_eq!(pending.tool, "write_object");

        // Approve it → the re-attempt of the same call now allows.
        store
            .pending_approvals()
            .resolve(ws.id, pending.id, ApprovalDecision::Approved)
            .await
            .unwrap();
        let v = gate.review(GatePhase::Call, &tool, &args, None, &ctx).await;
        assert!(matches!(v, GateVerdict::Allow));
        // The record was consumed; nothing unresolved remains.
        assert!(store
            .pending_approvals()
            .get_unresolved(ws.id, conv.id)
            .await
            .unwrap()
            .is_none());

        // A fresh ask + reject → the re-attempt denies.
        let v = gate.review(GatePhase::Call, &tool, &args, None, &ctx).await;
        assert!(matches!(v, GateVerdict::Defer { .. }));
        let p2 = store
            .pending_approvals()
            .get_unresolved(ws.id, conv.id)
            .await
            .unwrap()
            .unwrap();
        store
            .pending_approvals()
            .resolve(ws.id, p2.id, ApprovalDecision::Rejected)
            .await
            .unwrap();
        let v = gate.review(GatePhase::Call, &tool, &args, None, &ctx).await;
        assert!(matches!(v, GateVerdict::Deny { .. }));
    }

    #[tokio::test]
    async fn unparseable_and_erroring_scripts_fall_back_to_on_error() {
        let tool = StubTool {
            name: "x",
            cap: None,
        };
        // Returns garbage → unknown → on_error.
        let deny = script_gate("return 'maybe?';", GuardFail::Deny);
        assert!(matches!(
            verdict_for(&deny, &tool, json!({})).await,
            GateVerdict::Deny { .. }
        ));
        let allow = script_gate("return 'maybe?';", GuardFail::Allow);
        assert!(matches!(
            verdict_for(&allow, &tool, json!({})).await,
            GateVerdict::Allow
        ));
        // A throwing script → error → on_error (deny).
        let throwing = script_gate("throw new Error('boom');", GuardFail::Deny);
        assert!(matches!(
            verdict_for(&throwing, &tool, json!({})).await,
            GateVerdict::Deny { .. }
        ));
    }

    #[tokio::test]
    async fn mcp_read_only_policy_allows_reads_denies_writes() {
        // "This MCP server is read-only": deny any mcp call that isn't a read.
        let script = "if (input.mcp && !input.capability.read_only) return 'deny'; return 'allow';";
        let gate = script_gate(script, GuardFail::Deny);

        let read = StubTool {
            name: "wiki_search",
            cap: Some(Capability::new(Action::Read, Resource::new("mcp", "wiki"))),
        };
        let write = StubTool {
            name: "wiki_edit",
            cap: Some(Capability::new(Action::Write, Resource::new("mcp", "wiki"))),
        };
        // NB: the real registry gives MCP tools `mcp:use@server`; the guard sees the
        // declared capability, so a server that models read vs write is gateable.
        assert!(matches!(
            verdict_for(&gate, &read, json!({})).await,
            GateVerdict::Allow
        ));
        assert!(matches!(
            verdict_for(&gate, &write, json!({})).await,
            GateVerdict::Deny { .. }
        ));
    }

    #[tokio::test]
    async fn guard_input_object_is_null_when_no_file_is_referenced() {
        // A call with no key/path arg touches no object, so `input.object` is null and
        // `resolve_object` never queries the store (the lazy pool stays untouched).
        let tool = StubTool {
            name: "recall",
            cap: None,
        };
        let gate = script_gate("return input.object ? 'deny' : 'allow';", GuardFail::Deny);
        assert!(matches!(
            verdict_for(&gate, &tool, json!({ "query": "x" })).await,
            GateVerdict::Allow
        ));
    }

    /// End-to-end object-label gating against a real store (DB-gated, self-skips):
    /// a `deny`/`require_any` policy blocks/permits a storage call by the labels on
    /// the `(store, key)` it references.
    #[tokio::test]
    async fn object_label_policy_gates_by_the_files_labels() {
        let Some(url) = std::env::var("CATALERUM_TEST_DATABASE_URL")
            .or_else(|_| std::env::var("DATABASE_URL"))
            .ok()
        else {
            eprintln!("skipping object_label_policy_gates_…: set CATALERUM_TEST_DATABASE_URL");
            return;
        };
        use catalerum_core::model::{Author, ObjectLabelPolicy, ToolGuard};
        let store = Store::connect(&url).await.expect("connect+migrate");
        let ws = store
            .workspaces()
            .create("lg", &format!("lg-{}", uuid::Uuid::new_v4()))
            .await
            .expect("ws");
        let author = Author::User {
            id: catalerum_core::UserId::new(),
        };
        // Label `reports/q1.pdf` as `confidential` in the default store ("").
        store
            .object_labels()
            .add(ws.id, author, "", "reports/q1.pdf", false, "confidential")
            .await
            .expect("label");

        let gate = |policy: ObjectLabelPolicy| {
            ProfileToolGate::new(
                ToolGuard {
                    script: None,
                    llm: None,
                    object_labels: Some(policy),
                    on_error: GuardFail::Deny,
                },
                ToolRegistry::new(),
                store.clone(),
                ToolContext {
                    workspace_id: Some(ws.id),
                    ..Default::default()
                },
                OpenRouterClient::new("http://127.0.0.1:1", "k"),
                "m".into(),
                None,
            )
        };
        let tool = StubTool {
            name: "read_object",
            cap: Some(Capability::new(Action::Read, Resource::domain("storage"))),
        };
        let ctx = ToolContext {
            workspace_id: Some(ws.id),
            ..Default::default()
        };

        // deny-list blocks the confidential file.
        let deny_gate = gate(ObjectLabelPolicy {
            require_any: vec![],
            deny: vec!["confidential".into()],
        });
        let v = deny_gate
            .review(
                GatePhase::Call,
                &tool,
                &json!({ "key": "reports/q1.pdf" }),
                None,
                &ctx,
            )
            .await;
        assert!(matches!(v, GateVerdict::Deny { .. }));

        // require_any(shared) denies it (it lacks `shared`), but allows a labelled file.
        let req_gate = gate(ObjectLabelPolicy {
            require_any: vec!["shared".into()],
            deny: vec![],
        });
        let v = req_gate
            .review(
                GatePhase::Call,
                &tool,
                &json!({ "key": "reports/q1.pdf" }),
                None,
                &ctx,
            )
            .await;
        assert!(matches!(v, GateVerdict::Deny { .. }));

        // A different (unlabelled) file under a deny-only policy is allowed.
        let v = deny_gate
            .review(
                GatePhase::Call,
                &tool,
                &json!({ "key": "reports/public.pdf" }),
                None,
                &ctx,
            )
            .await;
        assert!(matches!(v, GateVerdict::Allow));
    }

    #[test]
    fn decision_parsing_is_lenient() {
        assert_eq!(normalize_decision("ALLOW"), "allow");
        assert_eq!(normalize_decision("Deny — writes to prod"), "deny");
        assert_eq!(normalize_decision("please ask the user"), "ask");
        assert_eq!(normalize_decision("require-user-feedback"), "ask");
        assert_eq!(normalize_decision("banana"), "unknown");

        let (d, r) =
            parse_decision("Sure! {\"decision\": \"deny\", \"reason\": \"prod write\"} done");
        assert_eq!(d, "deny");
        assert_eq!(r, "prod write");
        let (d, _) = parse_decision("I think we should allow this.");
        assert_eq!(d, "allow");
    }
}
