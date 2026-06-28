//! Running an [`AgentProfile`] as a scoped agent (SOUL §19/§25).
//!
//! A profile is the durable, named form of the §19 agent: a model + system
//! prompt + tool/skill set + the **subagents** it may delegate to + the §19
//! **grant** that is its authority, all within one workspace. This module turns a
//! stored profile into a running agent loop:
//!
//! - [`route_channel_to_profiles`] — the channel→profile inbound bridge (SOUL
//!   §25): every profile *listening* on a channel runs the §7 loop on an inbound
//!   message and replies back on that channel. Both inbound paths (the
//!   `POST /channels/{channel}/inbound` relay route and the live
//!   [`ChannelListener`](crate::ChannelListener)) call it.
//! - the **`delegate` tool** ([`DelegateTool`]) — a parent profile may hand a
//!   subtask to one of its configured subagents. The subagent runs under its
//!   **own** grant, enforced **⊆ the parent's** authority via
//!   [`attenuate`](catalerum_core::capability::attenuate) at delegation time — so
//!   a subagent can never widen data access beyond its caller (the §19
//!   attenuation invariant, the heart of "separate secure access to data").
//!
//! **Authority resolution.** A profile runs under its grant's capabilities (or,
//! grantless, bounded base-Member capabilities). A grant carrying a constraint the
//! runtime can't yet enforce (`env_allow`/`rate_limit`/`requires_approval`,
//! [`Constraints::has_unenforced`](catalerum_core::capability::Constraints::has_unenforced))
//! **fails closed** — the profile refuses to run rather than run with the
//! guardrail dropped, exactly like the automation executor (SOUL §19).
//!
//! **Recursion bound.** A subagent runs against the *base* registry (no `delegate`
//! tool), so delegation is one level deep — no infinite recursion.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::warn;

use catalerum_core::capability::{attenuate, Capability};
use catalerum_core::llm::{ChatMessage, ChatRequest};
use catalerum_core::model::{AgentProfile, Role, ToolGuard};
use catalerum_core::tool::{Tool, ToolContext, ToolRegistry};
use catalerum_core::WorkspaceId;
use catalerum_iam::base_capabilities;
use catalerum_llm::{run_agent, AgentConfig, OpenRouterClient};
use catalerum_store::Store;
use tokio_util::sync::CancellationToken;

use crate::subagent_runs::{
    register_subagent_run_tools, SubagentRunManager, SUBAGENT_CONTROL_TOOLS,
};

/// The tool name a profile's parent agent calls to delegate to a subagent.
pub(crate) const DELEGATE_TOOL: &str = "delegate";

/// Default system prompt for a profile that doesn't supply its own.
const DEFAULT_PROFILE_SYSTEM: &str = "You are a catalerum agent profile. Carry out the request \
using the tools available to you, stay within your authority, and stop when the task is done.";

/// The resolved, capability-scoped run context for a profile: the [`ToolContext`]
/// (its authority), the per-run [`ToolRegistry`] (the base registry plus the
/// always-on `delegate` tool), and the [`AgentConfig`] (carrying any grant cost
/// ceiling).
struct PreparedRun {
    ctx: ToolContext,
    registry: ToolRegistry,
    config: AgentConfig,
}

/// The parts of a durable profile that a purpose-built subagent launcher needs.
///
/// Computer and terminal subagents keep their deliberately tiny, pinned tool
/// registries, but may use a named profile for their model, instructions, tool
/// allow-list, guard, and attenuated authority. Keeping that resolution here
/// gives every subagent entry point the same fail-closed grant semantics as
/// `delegate` without letting a profile widen a constrained launcher's surface.
pub(crate) struct ConstrainedProfileRun {
    pub name: String,
    pub model: String,
    pub system: String,
    pub allowed_tools: Option<Vec<String>>,
    pub capabilities: Vec<Capability>,
    pub grant_id: Option<catalerum_core::GrantId>,
    pub dry_run: bool,
    pub cost_limit: Option<f64>,
    pub guard: Option<ToolGuard>,
}

/// Resolve a named profile for a constrained subagent and prove that its grant
/// is no broader than the caller's authority. Unknown profiles, unenforceable
/// grant constraints, and attempted escalation all fail closed.
pub(crate) async fn resolve_constrained_profile(
    store: &Store,
    workspace_id: WorkspaceId,
    default_model: &str,
    name: &str,
    parent_caps: &[Capability],
) -> catalerum_core::error::Result<ConstrainedProfileRun> {
    use catalerum_core::error::Error;

    let profile = store
        .agent_profiles()
        .get_by_name(workspace_id, name)
        .await
        .map_err(|error| Error::provider(format!("resolving subagent profile `{name}`: {error}")))?
        .ok_or(Error::NotFound)?;
    let (capabilities, dry_run, cost_limit) = match profile.grant_id {
        Some(grant_id) => {
            let grant = store
                .grants()
                .get(workspace_id, grant_id)
                .await
                .map_err(|error| {
                    Error::provider(format!("resolving subagent profile grant: {error}"))
                })?;
            if grant.constraints.has_unenforced() {
                return Err(Error::unauthorized(format!(
                    "subagent profile `{name}` grant carries an unenforceable constraint; refusing (fail-closed §19)"
                )));
            }
            (
                grant.capabilities,
                grant.constraints.dry_run,
                grant.constraints.cost_limit,
            )
        }
        None => (base_capabilities(Role::Member), false, None),
    };
    if !subagent_within_parent(parent_caps, &capabilities) {
        return Err(Error::unauthorized(format!(
            "subagent profile `{name}` requires authority its caller does not hold (attenuation §19)"
        )));
    }

    let runbooks = skill_runbooks(store, workspace_id, &profile.skills).await;
    let base = profile
        .system_prompt
        .clone()
        .unwrap_or_else(|| DEFAULT_PROFILE_SYSTEM.to_string());
    Ok(ConstrainedProfileRun {
        name: profile.name,
        model: profile.model.unwrap_or_else(|| default_model.to_string()),
        system: compose_system(&base, &runbooks),
        allowed_tools: allow_opt(&profile.tools),
        capabilities,
        grant_id: profile.grant_id,
        dry_run,
        cost_limit,
        guard: profile.guard,
    })
}

/// Run a resolved profile's §7 agent loop on `user_text`, returning the outcome —
/// the single entry point both the durable `run_profile` job and the `RunProfile`
/// automation action drive (via [`crate::action_runner`]) and the channel/chat
/// surfaces reach.
///
/// `ceiling_caps`, when `Some`, is an **attenuation ceiling** (e.g. the invoking
/// automation's grant): the profile's own authority must be **⊆** it, else the run
/// is refused (a profile can never be used to escalate its caller). `None` runs the
/// profile under its own grant with no further ceiling (the channel-listener case —
/// the profile's grant *is* the authority). When `reply_channel` is `Some`, the
/// final reply is delivered there via the `notify` tool under the profile's own
/// context (so a profile without `channel:write` simply can't reply, §19).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_profile(
    store: &Store,
    client: &OpenRouterClient,
    registry: &ToolRegistry,
    default_model: &str,
    profile: &AgentProfile,
    user_text: &str,
    reply_channel: Option<&str>,
    ceiling_caps: Option<&[Capability]>,
) -> Result<catalerum_llm::AgentOutcome, String> {
    let prep = prepare_run(
        store,
        client,
        registry,
        default_model,
        profile,
        ceiling_caps,
    )
    .await?;
    let (request, allowed) = build_request(store, default_model, profile, user_text).await;
    let outcome = run_agent(
        client,
        request,
        &prep.registry,
        &prep.ctx,
        &prep.config,
        allowed.as_deref(),
    )
    .await
    .map_err(|e| format!("profile `{}` agent loop failed: {e}", profile.name))?;
    if let Some(channel) = reply_channel {
        let reply = outcome.content.trim();
        if !reply.is_empty() {
            // Deliver via the same `notify` path the automation auto-reply uses,
            // under the profile's resolved context (channel:write enforced).
            let args = json!({ "channel": channel, "message": reply });
            if let Err(e) = prep.registry.dispatch(NOTIFY_TOOL, args, &prep.ctx).await {
                warn!(profile = %profile.name, channel = %channel, error = %e, "profile channel reply failed");
            }
        }
    }
    Ok(outcome)
}

/// The registry tool that delivers a reply to a channel (SOUL §25).
const NOTIFY_TOOL: &str = "notify";

/// Chat overrides for a user "running a thread as" a profile (SOUL §19/§12): the
/// model, the persona+skills system prompt, the tool allow-list, and the
/// capabilities the chat loop runs under.
pub(crate) struct ChatProfileRun {
    /// The profile's authority **intersected with the user's own** — so the thread
    /// can only *narrow* the user's access, never escalate it.
    pub capabilities: Vec<Capability>,
    /// The profile's model (or the workspace default).
    pub model: String,
    /// The profile's persona system prompt + its skills' runbooks.
    pub system: String,
    /// The profile's tool allow-list (`None` = advertise the whole registry).
    pub allowed_tools: Option<Vec<String>>,
    /// The profile's subagent allow-list for delegation (empty = any workspace
    /// profile; ephemeral workers always available).
    pub subagents: Vec<String>,
    /// The profile's optional tool guard (SOUL §19), built into a
    /// [`ToolGate`](catalerum_core::tool::ToolGate) for the chat run.
    pub guard: Option<ToolGuard>,
}

/// A profile's name list as a tool allow-list: empty → `None` (unrestricted), a
/// non-empty list → `Some(clone)`. Shared by the tool allow-list shaping.
fn allow_opt(names: &[String]) -> Option<Vec<String>> {
    if names.is_empty() {
        None
    } else {
        Some(names.to_vec())
    }
}

/// Resolve a bound profile into chat overrides for a user "running as" it (SOUL
/// §19). The capabilities are the profile's own authority (its grant, or base
/// Member) **intersected with `user_caps`** (the user's role) — a profile can
/// *scope down* a chat (a calendar-only thread) but **never escalate** the user
/// beyond what they already hold. The model, persona prompt (+ skill runbooks), and
/// tool allow-list come from the profile.
pub(crate) async fn resolve_chat_profile(
    store: &Store,
    default_model: &str,
    profile: &AgentProfile,
    user_caps: &[Capability],
) -> ChatProfileRun {
    let profile_caps = match profile.grant_id {
        Some(gid) => match store.grants().get(profile.workspace_id, gid).await {
            Ok(g) => g.capabilities,
            Err(e) => {
                warn!(profile = %profile.name, error = %e, "loading profile grant for chat; using base Member");
                base_capabilities(Role::Member)
            }
        },
        None => base_capabilities(Role::Member),
    };
    // Intersection: keep only profile capabilities the user's role already covers —
    // never an escalation (an Owner's `*` keeps all; a Member keeps the overlap).
    let capabilities = profile_caps
        .into_iter()
        .filter(|cap| user_caps.iter().any(|u| u.covers(cap)))
        .collect();
    let runbooks = skill_runbooks(store, profile.workspace_id, &profile.skills).await;
    let base = profile
        .system_prompt
        .clone()
        .unwrap_or_else(|| DEFAULT_PROFILE_SYSTEM.to_string());
    let system = compose_system(&base, &runbooks);
    let model = profile
        .model
        .clone()
        .unwrap_or_else(|| default_model.to_string());
    let allowed_tools = allow_opt(&profile.tools);
    ChatProfileRun {
        capabilities,
        model,
        system,
        allowed_tools,
        subagents: profile.subagents.clone(),
        guard: profile.guard.clone(),
    }
}

/// Resolve a profile's authority into a scoped run context + per-run registry.
///
/// The capabilities come from the profile's grant (or bounded base-Member when
/// grantless). A grant carrying an unenforceable constraint **fails closed**
/// (SOUL §19). When `ceiling_caps` is supplied, the profile's authority must be
/// **⊆** it (attenuation) — else the run is refused. When the profile has
/// subagents, the per-run registry gets a `delegate` tool scoped to exactly those
/// subagents + this profile's authority.
async fn prepare_run(
    store: &Store,
    client: &OpenRouterClient,
    registry: &ToolRegistry,
    default_model: &str,
    profile: &AgentProfile,
    ceiling_caps: Option<&[Capability]>,
) -> Result<PreparedRun, String> {
    let ws = profile.workspace_id;
    let (caps, dry_run, cost_limit) = match profile.grant_id {
        Some(gid) => {
            let grant = store
                .grants()
                .get(ws, gid)
                .await
                .map_err(|e| format!("resolving grant for profile `{}`: {e}", profile.name))?;
            if grant.constraints.has_unenforced() {
                return Err(format!(
                    "profile `{}` grant carries a constraint the runtime can't yet enforce; \
                     refusing to run (fail-closed §19)",
                    profile.name
                ));
            }
            (
                grant.capabilities.clone(),
                grant.constraints.dry_run,
                grant.constraints.cost_limit,
            )
        }
        None => (base_capabilities(Role::Member), false, None),
    };
    // Attenuation (§19): when invoked under a ceiling (an automation's grant), the
    // profile's authority must be ⊆ it — a profile can't escalate its caller.
    if let Some(ceiling) = ceiling_caps {
        if !subagent_within_parent(ceiling, &caps) {
            return Err(format!(
                "profile `{}` authority exceeds the caller's (attenuation §19); refusing to run",
                profile.name
            ));
        }
    }
    let mut ctx = ToolContext {
        workspace_id: Some(ws),
        user_id: None,
        agent_id: None,
        grant_id: profile.grant_id,
        capabilities: Some(caps.clone()),
        dry_run,
        gate: None,
        conversation_id: None,
        ui_id: None,
        registry: None,
    };
    // A guarded profile runs its tool calls through its classifier (SOUL §19). This
    // is the headless path (automation `RunProfile`, channel listener): `ctx` has no
    // `conversation_id`, so an `ask` (require-user-feedback) fails closed — there is
    // no interactive thread to defer a durable approval onto.
    let caller_model = profile
        .model
        .clone()
        .unwrap_or_else(|| default_model.to_string());
    ctx.gate = crate::tool_gate::build_gate(
        profile.guard.as_ref(),
        registry.clone(),
        store.clone(),
        ctx.clone(),
        client.clone(),
        caller_model.clone(),
    );
    // Delegation is on by default (SOUL §19): every profile run gets a `delegate`
    // tool — ephemeral workers + (its `subagents`, empty = any workspace profile).
    // The caller model an ephemeral worker inherits is the profile's own model
    // (`caller_model`, resolved above for the guard).
    let run_registry = registry_with_delegate(
        registry,
        store,
        client,
        ws,
        default_model,
        &caller_model,
        caps,
        profile.subagents.clone(),
        // Headless run — no conversation, so a subagent `ask` fails closed.
        ctx.conversation_id,
        // A headless parent gets a run-local manager. Its derived registry below
        // carries matching control tools, so it can monitor/wait/stop during this
        // run without exposing handles to unrelated AppState conversations.
        SubagentRunManager::default(),
    );
    Ok(PreparedRun {
        ctx,
        registry: run_registry,
        config: AgentConfig {
            cost_limit,
            // Deferred advertising (SOUL §7): an unconfined profile starts from
            // the discovery tools + `delegate` and `search_models` (the standing
            // nudge references both) and loads the rest on demand; an explicit
            // tool list keeps full advertising of that set.
            discovery_tools: if profile.tools.is_empty() {
                let mut seed = crate::tools::discovery_seed();
                append_delegate_support_tools(&mut seed);
                seed
            } else {
                Vec::new()
            },
            ..AgentConfig::default()
        },
    })
}

/// Clone `base` and register the `delegate` tool scoped to a caller's authority +
/// model + (optional) subagent allow-list — so an agent can spin ephemeral workers
/// and delegate to named profiles, always **⊆ itself** (SOUL §19). Used by every
/// agent run (profiles + chat) to put delegation **on by default**. An empty
/// `allowed_subagents` permits any profile in the workspace.
#[allow(clippy::too_many_arguments)]
pub(crate) fn registry_with_delegate(
    base: &ToolRegistry,
    store: &Store,
    client: &OpenRouterClient,
    workspace_id: WorkspaceId,
    default_model: &str,
    caller_model: &str,
    caller_caps: Vec<Capability>,
    allowed_subagents: Vec<String>,
    conversation_id: Option<catalerum_core::ConversationId>,
    subagent_runs: SubagentRunManager,
) -> ToolRegistry {
    let mut r = base.clone();
    // Override the base registry's lifecycle tools with this derived run's
    // manager. Interactive chat passes AppState's shared manager; headless
    // profiles pass a run-local manager.
    register_subagent_run_tools(&mut r, subagent_runs.clone());
    r.register(Arc::new(DelegateTool {
        store: store.clone(),
        client: client.clone(),
        base_registry: base.clone(),
        default_model: default_model.to_string(),
        caller_model: caller_model.to_string(),
        workspace_id,
        allowed_subagents,
        parent_caps: caller_caps,
        conversation_id,
        subagent_runs,
        run_cancel: None,
    }));
    r
}

/// Build the seed [`ChatRequest`] + advertised tool subset for a profile run. The
/// system prompt is the profile's (or a default) plus its skills' runbooks and
/// standing delegation guidance (delegation is on by default, SOUL §19); the model
/// defaults to `default_model` unless the profile pins one; an empty `tools` list
/// advertises the whole registry, while a non-empty list confines the loop to that
/// subset (always **with `delegate` added**, so a confined profile can still delegate).
async fn build_request(
    store: &Store,
    default_model: &str,
    profile: &AgentProfile,
    user_text: &str,
) -> (ChatRequest, Option<Vec<String>>) {
    let base = profile
        .system_prompt
        .clone()
        .unwrap_or_else(|| DEFAULT_PROFILE_SYSTEM.to_string());
    let runbooks = skill_runbooks(store, profile.workspace_id, &profile.skills).await;
    let system = format!(
        "{}\n\n{}",
        compose_system(&base, &runbooks),
        crate::guidance::DELEGATE_GUIDANCE
    );
    let model = profile
        .model
        .clone()
        .unwrap_or_else(|| default_model.to_string());
    let seed = vec![
        ChatMessage::system(system),
        ChatMessage::user(user_text.to_string()),
    ];
    let allowed = if profile.tools.is_empty() {
        None
    } else {
        let mut t = profile.tools.clone();
        append_delegate_support_tools(&mut t);
        Some(t)
    };
    (ChatRequest::new(model, seed), allowed)
}

/// System prompt for an **ephemeral worker** subagent (no named profile).
const DEFAULT_WORKER_SYSTEM: &str = "You are a focused worker subagent. Complete the single \
task you are given using the tools available to you, stay within your authority, and return \
only the result — concise and self-contained, since your caller sees only your final message, \
not your intermediate steps.";

/// Add the launcher plus every standing support tool promised by
/// [`crate::guidance::DELEGATE_GUIDANCE`] to an advertised tool list, without
/// duplicates.
pub(crate) fn append_delegate_support_tools(tools: &mut Vec<String>) {
    for name in std::iter::once(DELEGATE_TOOL)
        .chain(std::iter::once(crate::tools::SEARCH_MODELS_NAME))
        .chain(SUBAGENT_CONTROL_TOOLS)
    {
        if !tools.iter().any(|existing| existing == name) {
            tools.push(name.to_string());
        }
    }
}

/// The `delegate` tool: hand a focused subtask to a **subagent** so it runs in its
/// own fresh context window and returns only its result — keeping the caller's
/// context lean and (with a cheaper `model`) cheaper. Two modes, both **⊆ the
/// caller's authority** (attenuation §19) and **one level deep** (a subagent runs
/// against the base registry, with no `delegate` tool, so there is no recursion):
/// - **named profile** (`profile` given) — runs an [`AgentProfile`] under its own
///   grant, verified ⊆ the caller; restricted to `allowed_subagents` when that list
///   is non-empty, else **any** profile in the workspace (delegation is default-on).
/// - **ephemeral worker** (`profile` omitted) — a throwaway agent under the
///   *caller's own* authority, with an optional `model` override + `tools` subset.
///   Zero configuration: the default cost/context play.
#[derive(Clone)]
struct DelegateTool {
    store: Store,
    client: OpenRouterClient,
    /// The registry a subagent runs against — the *base* registry (no `delegate`
    /// tool), so delegation is one level deep (no recursion).
    base_registry: ToolRegistry,
    /// Workspace default model (for a named profile with no model of its own).
    default_model: String,
    /// The caller's model — an ephemeral worker inherits it unless the call
    /// overrides it (the "same model as caller" default).
    caller_model: String,
    workspace_id: WorkspaceId,
    /// Profile names this caller may delegate to. **Empty = any profile in the
    /// workspace** (delegation is on by default; the list is an optional tightening).
    allowed_subagents: Vec<String>,
    /// The caller's capabilities — the ceiling every subagent stays ⊆.
    parent_caps: Vec<Capability>,
    /// The conversation this run belongs to (SOUL §19), inherited by a delegated
    /// subagent so its guard can defer a durable approval onto the same thread.
    /// `None` on a headless run — a subagent `ask` fails closed there.
    conversation_id: Option<catalerum_core::ConversationId>,
    /// Shared with the parent-facing lifecycle tools in this derived registry.
    subagent_runs: SubagentRunManager,
    /// Set only on the private tool clone owned by a background run.
    run_cancel: Option<CancellationToken>,
}

impl DelegateTool {
    /// Delegate to a **named profile**: resolve it (allow-list permitting), run it
    /// under its own grant verified ⊆ the caller (attenuation §19).
    async fn delegate_to_profile(
        &self,
        name: &str,
        task: &str,
        model_override: Option<String>,
        caller: &ToolContext,
    ) -> catalerum_core::error::Result<Value> {
        use catalerum_core::error::Error;
        // An explicit allow-list restricts; an empty one permits any workspace profile.
        if !self.allowed_subagents.is_empty() && !self.allowed_subagents.iter().any(|s| s == name) {
            return Err(Error::unauthorized(format!(
                "`{name}` is not among this agent's permitted subagents"
            )));
        }
        let sub = self
            .store
            .agent_profiles()
            .get_by_name(self.workspace_id, name)
            .await
            .map_err(|e| Error::provider(format!("resolving subagent `{name}`: {e}")))?
            .ok_or(Error::NotFound)?;
        // The subagent's effective authority: its grant (fail-closed on an
        // unenforceable constraint), or bounded base-Member when grantless.
        let (sub_caps, dry_run, cost_limit) = match sub.grant_id {
            Some(gid) => {
                let g = self
                    .store
                    .grants()
                    .get(self.workspace_id, gid)
                    .await
                    .map_err(|e| Error::provider(format!("resolving subagent grant: {e}")))?;
                if g.constraints.has_unenforced() {
                    return Err(Error::unauthorized(format!(
                        "subagent `{name}` grant carries an unenforceable constraint; refusing \
                         (fail-closed §19)"
                    )));
                }
                (
                    g.capabilities.clone(),
                    g.constraints.dry_run,
                    g.constraints.cost_limit,
                )
            }
            None => (base_capabilities(Role::Member), false, None),
        };
        // ATTENUATION (§19): every subagent capability must be covered by a caller
        // capability — a subagent can never hold authority its caller lacks.
        if !subagent_within_parent(&self.parent_caps, &sub_caps) {
            return Err(Error::unauthorized(format!(
                "subagent `{name}` requires authority its caller does not hold (attenuation §19)"
            )));
        }
        let mut sub_ctx = ToolContext {
            workspace_id: Some(self.workspace_id),
            // Inherit the caller's acting principal so author-recording tools
            // (present_ui, create_note, …) attribute created content to the human/agent
            // behind the delegation chain rather than an anonymous principal (which
            // `author()` rejects). The subagent's *authority* is still its own grant
            // (`capabilities`/`grant_id` below), so this attributes without escalating.
            user_id: caller.user_id,
            agent_id: caller.agent_id,
            grant_id: sub.grant_id,
            capabilities: Some(sub_caps),
            dry_run,
            gate: None,
            // Inherit the caller's conversation so a guarded subagent can defer a
            // durable approval onto the same thread (SOUL §19); `None` off the chat
            // path, where a subagent `ask` fails closed.
            conversation_id: self.conversation_id,
            ui_id: None,
            registry: None,
        };
        let base = sub
            .system_prompt
            .clone()
            .unwrap_or_else(|| DEFAULT_PROFILE_SYSTEM.to_string());
        let runbooks = skill_runbooks(&self.store, self.workspace_id, &sub.skills).await;
        let system = compose_system(&base, &runbooks);
        // A per-call `model` overrides; else the profile's own model; else default.
        let model = model_override
            .or_else(|| sub.model.clone())
            .unwrap_or_else(|| self.default_model.clone());
        // A guarded subagent runs its own tool calls through its classifier; an
        // `ask` defers a durable approval onto the inherited conversation.
        sub_ctx.gate = crate::tool_gate::build_gate(
            sub.guard.as_ref(),
            self.base_registry.clone(),
            self.store.clone(),
            sub_ctx.clone(),
            self.client.clone(),
            model.clone(),
        );
        let allowed = if sub.tools.is_empty() {
            None
        } else {
            Some(sub.tools.clone())
        };
        let outcome = run_agent(
            &self.client,
            ChatRequest::new(
                model,
                vec![
                    ChatMessage::system(system),
                    ChatMessage::user(task.to_string()),
                ],
            ),
            &self.base_registry,
            &sub_ctx,
            &AgentConfig {
                cost_limit,
                cancel: self.run_cancel.clone().unwrap_or_default(),
                // Deferred advertising (SOUL §7); the base registry carries no
                // `delegate` (one level deep), so the seed is just discovery.
                discovery_tools: if allowed.is_none() {
                    crate::tools::discovery_seed()
                } else {
                    Vec::new()
                },
                ..AgentConfig::default()
            },
            allowed.as_deref(),
        )
        .await
        .map_err(|e| Error::provider(format!("subagent `{name}` loop failed: {e}")))?;
        Ok(json!({
            "subagent": name,
            "content": outcome.content,
            "tool_calls": outcome.tool_invocations.len(),
            "stopped": outcome.stopped,
        }))
    }

    /// Spin an **ephemeral worker** under the *caller's own* authority (⊆ trivially):
    /// a throwaway focused agent with an optional model + tool subset, run in its own
    /// context. No configuration; the default cost/context play.
    async fn run_ephemeral(
        &self,
        task: &str,
        model_override: Option<String>,
        tool_override: Option<Vec<String>>,
        caller: &ToolContext,
    ) -> catalerum_core::error::Result<Value> {
        use catalerum_core::error::Error;
        // The worker runs under the caller's *own* authority, so it acts *as* the
        // caller: inherit the acting principal (user/agent) and the dry-run posture.
        // Without a principal, author-recording tools (present_ui, create_note, …)
        // fail with "tool call has no acting principal" (`author()`).
        let ctx = ToolContext {
            workspace_id: Some(self.workspace_id),
            user_id: caller.user_id,
            agent_id: caller.agent_id,
            grant_id: None,
            capabilities: Some(self.parent_caps.clone()),
            dry_run: caller.dry_run,
            gate: None,
            conversation_id: None,
            ui_id: None,
            registry: None,
        };
        let model = model_override.unwrap_or_else(|| self.caller_model.clone());
        // An explicit, non-empty tool subset confines the worker; otherwise it sees
        // the whole registry (capabilities still gate every call).
        let allowed = tool_override.filter(|t| !t.is_empty());
        let outcome = run_agent(
            &self.client,
            ChatRequest::new(
                model,
                vec![
                    ChatMessage::system(DEFAULT_WORKER_SYSTEM.to_string()),
                    ChatMessage::user(task.to_string()),
                ],
            ),
            &self.base_registry,
            &ctx,
            &AgentConfig {
                cancel: self.run_cancel.clone().unwrap_or_default(),
                // Deferred advertising (SOUL §7) for an unconfined worker; an
                // explicit tool subset keeps full advertising of that set.
                discovery_tools: if allowed.is_none() {
                    crate::tools::discovery_seed()
                } else {
                    Vec::new()
                },
                ..AgentConfig::default()
            },
            allowed.as_deref(),
        )
        .await
        .map_err(|e| Error::provider(format!("ephemeral worker loop failed: {e}")))?;
        Ok(json!({
            "worker": true,
            "content": outcome.content,
            "tool_calls": outcome.tool_invocations.len(),
            "stopped": outcome.stopped,
        }))
    }
}

#[async_trait]
impl Tool for DelegateTool {
    fn name(&self) -> &str {
        DELEGATE_TOOL
    }

    fn description(&self) -> &str {
        "Delegate a focused subtask to a subagent that runs in its own fresh context window and \
         returns only its result — keeping your context lean and (with a cheaper `model`) cheaper. \
         Omit `profile` to spin an ephemeral worker under your own authority; pass `profile` to use \
         a named agent profile. A subagent can never exceed your authority. Set `background=true` \
         to return a run id immediately, then use monitor_subagent, wait_subagent, or \
         stop_subagent from the parent."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "The subtask for the subagent. It runs in its own context and returns only its final result, so make it self-contained."
                },
                "profile": {
                    "type": "string",
                    "description": "Optional: delegate to a named agent profile. Omit to spin an ephemeral worker under your own authority."
                },
                "model": {
                    "type": "string",
                    "description": "Optional model id for the subagent — use a cheaper/faster model for routine subtasks to reduce cost. Defaults to your model. Use `search_models` to find available model ids by name."
                },
                "tools": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional: restrict an ephemeral worker to these tool names (ignored for a named profile)."
                },
                "background": {
                    "type": "boolean",
                    "default": false,
                    "description": "Run the delegation on this API pod and return a controllable run id immediately."
                }
            },
            "required": ["task"]
        })
    }

    async fn invoke(&self, args: Value, ctx: &ToolContext) -> catalerum_core::error::Result<Value> {
        use catalerum_core::error::Error;
        let task = args
            .get("task")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default()
            .to_string();
        if task.is_empty() {
            return Err(Error::invalid("delegate requires a non-empty `task`"));
        }
        if self.run_cancel.is_none()
            && args
                .get("background")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        {
            let profile = args
                .get("profile")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty());
            let label = match profile {
                Some(profile) => format!("{profile}: {task}"),
                None => task.clone(),
            }
            .chars()
            .take(160)
            .collect::<String>();
            let mut run_args = args;
            if let Some(object) = run_args.as_object_mut() {
                object.remove("background");
            }
            let template = self.clone();
            let parent_ctx = ctx.clone();
            return self
                .subagent_runs
                .spawn(ctx, DELEGATE_TOOL, label, move |cancel| async move {
                    let runner: Arc<dyn Tool> = Arc::new(Self {
                        run_cancel: Some(cancel),
                        ..template
                    });
                    runner.invoke(run_args, &parent_ctx).await
                })
                .await;
        }
        let model_override = args
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let profile_name = args
            .get("profile")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        match profile_name {
            Some(name) => {
                self.delegate_to_profile(&name, &task, model_override, ctx)
                    .await
            }
            None => {
                let tools = args.get("tools").and_then(Value::as_array).map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                });
                // An ephemeral worker inherits the caller's principal + dry-run posture.
                self.run_ephemeral(&task, model_override, tools, ctx).await
            }
        }
    }
}

/// The attenuation gate (SOUL §19): a subagent's authority is valid only if every
/// one of its capabilities is covered by some parent capability. Pure, so the
/// invariant is unit-testable without a live LLM/store.
#[must_use]
fn subagent_within_parent(parent: &[Capability], subagent: &[Capability]) -> bool {
    subagent.iter().all(|cap| attenuate(parent, cap).is_ok())
}

/// Load the `instructions_md` runbooks of `names` in `ws` (SOUL §23), in order,
/// skipping any missing. Empty when no names are given.
async fn skill_runbooks(store: &Store, ws: WorkspaceId, names: &[String]) -> Vec<String> {
    if names.is_empty() {
        return Vec::new();
    }
    let by_name: HashMap<String, _> = match store.skills().get_many_by_name(ws, names).await {
        Ok(found) => found.into_iter().map(|s| (s.name.clone(), s)).collect(),
        Err(e) => {
            warn!(error = %e, "loading profile skills failed; skipping all");
            return Vec::new();
        }
    };
    names
        .iter()
        .filter_map(|n| by_name.get(n).map(|s| s.instructions_md.clone()))
        .collect()
}

/// Compose a system prompt from a base + skill runbooks (SOUL §23).
fn compose_system(base: &str, runbooks: &[String]) -> String {
    if runbooks.is_empty() {
        return base.to_string();
    }
    let mut s = base.to_string();
    s.push_str("\n\n# Skills\n\nYou have been given these runbooks; follow them:\n");
    for r in runbooks {
        s.push_str("\n---\n");
        s.push_str(r);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use catalerum_core::capability::{Action, Resource};

    fn cap(action: Action, domain: &str, sel: Option<&str>) -> Capability {
        match sel {
            Some(s) => Capability::new(action, Resource::new(domain, s)),
            None => Capability::new(action, Resource::domain(domain)),
        }
    }

    #[test]
    fn subagent_within_parent_enforces_attenuation() {
        let parent = vec![
            cap(Action::Read, "calendar", None),
            cap(Action::Write, "storage", Some("local/out/*")),
        ];

        // ⊆ parent: read calendar, narrower storage write → allowed.
        assert!(subagent_within_parent(
            &parent,
            &[
                cap(Action::Read, "calendar", None),
                cap(Action::Write, "storage", Some("local/out/report.pdf")),
            ]
        ));

        // An empty subagent (no authority) is trivially ⊆ any parent.
        assert!(subagent_within_parent(&parent, &[]));

        // Escalation by action: parent can't write calendar → rejected.
        assert!(!subagent_within_parent(
            &parent,
            &[cap(Action::Write, "calendar", None)]
        ));

        // Escalation by domain: parent has no email authority at all → rejected.
        assert!(!subagent_within_parent(
            &parent,
            &[cap(Action::Read, "email", None)]
        ));

        // Escalation by selector: parent's storage write is scoped to local/out/* →
        // a broader local/* subagent escapes it → rejected.
        assert!(!subagent_within_parent(
            &parent,
            &[cap(Action::Write, "storage", Some("local/*"))]
        ));
    }

    #[test]
    fn compose_system_appends_runbooks_or_leaves_base() {
        assert_eq!(compose_system("base", &[]), "base");
        let composed = compose_system("base", &["RUNBOOK".to_string()]);
        assert!(composed.starts_with("base"));
        assert!(composed.contains("RUNBOOK"));
        assert!(composed.contains("# Skills"));
    }

    #[test]
    fn delegate_support_tools_include_background_lifecycle_without_duplicates() {
        let mut tools = vec![DELEGATE_TOOL.to_string()];
        append_delegate_support_tools(&mut tools);
        append_delegate_support_tools(&mut tools);
        for name in std::iter::once(DELEGATE_TOOL)
            .chain(std::iter::once(crate::tools::SEARCH_MODELS_NAME))
            .chain(SUBAGENT_CONTROL_TOOLS)
        {
            assert_eq!(tools.iter().filter(|tool| tool.as_str() == name).count(), 1);
        }
    }
}
