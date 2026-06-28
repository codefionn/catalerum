//! `computer_*` — drive an enrolled **computer agent** (SOUL §19/§20).
//!
//! A computer agent is a daemon a user installs on a server/desktop; it dials into
//! `GET /computer-agents/connect` and serves scoped file / search / exec / desktop
//! operations over the pod-local [`ComputerRegistry`](crate::computer_registry).
//! These tools are the LLM's client of that surface — each resolves the target
//! machine (the single online agent, or the one named in `agent`), issues one
//! [`ComputerOp`], and returns the agent's result. The agent enforces its own
//! directory scope and exec policy locally, so a compromised server can never
//! widen what a machine serves.
//!
//! **Authority (SOUL §19).** Controlling a host is a *protected* scope, like the
//! Local executor: every tool gates on the `computer` domain, which is not a
//! standard member domain — so only Owner/Admin (via their `*` wildcard) hold it.
//! Reads gate on `computer:read`, file writes on `computer:write`, and command /
//! desktop / grant-access / sub-agent control on `computer:run`.
//!
//! **Constrained computer subagent.** `computer_subagent` gets a fresh tool
//! registry containing only the direct controls for one pinned machine plus an
//! `upstream` tool. The calling chat defines that tool as a bounded Boa handler:
//! the child submits JSON requests, while the handler may selectively transform
//! parent-provided context and call tools from the parent's exact registry under
//! the parent's original authority. Thus the child never receives the parent
//! registry or workspace information tools directly; the handler is the sole
//! information boundary.
//!
//! **Command safety — "auto mode".** `computer_exec` runs each command through the
//! machine's [`ExecPolicy`]: `always_allow` runs it, `deny` refuses, `always_ask`
//! always requires a human's approval, and `auto` consults a one-shot LLM
//! classifier that rules `allow` / `deny` / `ask`. An `ask` (and `always_ask`)
//! records a durable [`PendingApproval`](catalerum_core::model::PendingApproval)
//! and defers — the same server-enforced Approve/Reject flow the profile tool guard
//! uses, so the model can never self-approve and a run with no interactive
//! conversation fails closed.

use std::time::Duration;

use super::*;

use catalerum_core::computer::{
    ComputerCapabilities, ComputerOp, DesktopAction, DirMode, WriteMode, DEFAULT_EXEC_TIMEOUT_SECS,
    DEFAULT_SEARCH_TIMEOUT_SECS, MAX_EXEC_TIMEOUT_SECS, MAX_SEARCH_TIMEOUT_SECS,
};
use catalerum_core::model::ApprovalDecision;
use catalerum_core::{ChatMessage, ChatRequest, ComputerAgentId, WorkspaceId};
use catalerum_llm::{run_agent, AgentConfig};
use tokio_util::sync::CancellationToken;

use crate::computer_registry::{ComputerRegistry, DEFAULT_OP_TIMEOUT};
use crate::profile_agent::{resolve_constrained_profile, ConstrainedProfileRun};
use crate::subagent_runs::SubagentRunManager;

async fn selected_subagent_profile(
    store: &Store,
    default_model: &str,
    args: &Json,
    ctx: &ToolContext,
    workspace_id: WorkspaceId,
) -> Result<Option<ConstrainedProfileRun>> {
    let Some(name) = opt_str_some(args, "profile") else {
        return Ok(None);
    };
    let parent_caps = ctx.capabilities.as_deref().ok_or_else(|| {
        Error::unauthorized("a named subagent profile requires a capability-scoped caller")
    })?;
    resolve_constrained_profile(store, workspace_id, default_model, &name, parent_caps)
        .await
        .map(Some)
}

/// Register every `computer_*` tool into `registry`. Called from
/// [`AppState::new`](crate::state::AppState) with the shared live registry.
pub(crate) fn register_computer_tools(
    registry: &mut ToolRegistry,
    store: Store,
    client: OpenRouterClient,
    computer: Arc<ComputerRegistry>,
    default_model: String,
    subagent_runs: SubagentRunManager,
) {
    add_direct_computer_tools(registry, &store, &client, &computer, &default_model);
    // The delegate-style "give the machine a whole task" tool runs a subagent over
    // a registry of the *direct* tools only (one level deep — no nested task tool).
    registry.register(Arc::new(ComputerAgentTaskTool {
        store: store.clone(),
        client: client.clone(),
        computer: computer.clone(),
        default_model: default_model.clone(),
    }));
    registry.register(Arc::new(ComputerSubagentTool {
        store,
        client,
        computer,
        default_model,
        subagent_runs,
        run_cancel: None,
    }));
}

/// Register the direct (non-subagent) computer tools into `registry`. Shared by the
/// main registration and the `computer_agent_task` subagent's restricted registry.
fn add_direct_computer_tools(
    registry: &mut ToolRegistry,
    store: &Store,
    client: &OpenRouterClient,
    computer: &Arc<ComputerRegistry>,
    default_model: &str,
) {
    registry.register(Arc::new(ComputerListAgentsTool {
        store: store.clone(),
        computer: computer.clone(),
    }));
    registry.register(Arc::new(ComputerListDirTool {
        computer: computer.clone(),
    }));
    registry.register(Arc::new(ComputerReadFileTool {
        computer: computer.clone(),
    }));
    registry.register(Arc::new(ComputerStatTool {
        computer: computer.clone(),
    }));
    registry.register(Arc::new(ComputerSearchTool {
        computer: computer.clone(),
    }));
    registry.register(Arc::new(ComputerWriteFileTool {
        computer: computer.clone(),
    }));
    registry.register(Arc::new(ComputerRequestAccessTool {
        store: store.clone(),
        computer: computer.clone(),
    }));
    registry.register(Arc::new(ComputerExecTool {
        store: store.clone(),
        client: client.clone(),
        computer: computer.clone(),
        default_model: default_model.to_string(),
    }));
    registry.register(Arc::new(ComputerDesktopTool {
        computer: computer.clone(),
    }));
}

/// A build-a-registry helper for the `computer_agent_task` subagent: only the
/// direct tools, so the subagent drives the machine but cannot recurse into another
/// task delegation.
fn direct_computer_registry(
    store: &Store,
    client: &OpenRouterClient,
    computer: &Arc<ComputerRegistry>,
    default_model: &str,
) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    add_direct_computer_tools(&mut registry, store, client, computer, default_model);
    registry
}

/// Direct controls useful to a worker that has already been assigned one
/// machine. `computer_list_agents` is deliberately absent: the parent resolved
/// the machine, and the child has no reason to enumerate the workspace's other
/// enrolled hosts.
const SUBAGENT_COMPUTER_TOOLS: &[&str] = &[
    "computer_list_dir",
    "computer_read_file",
    "computer_stat",
    "computer_search",
    "computer_write_file",
    "computer_request_access",
    "computer_exec",
    "computer_desktop",
];

/// Restrict a direct `computer_*` tool to the machine chosen by the parent. The
/// wrapper also removes the now-meaningless `agent` input from the advertised
/// schema. Capability checks still happen at the restricted registry's dispatch
/// boundary because the wrapper exposes the inner tool's required capability.
struct PinnedComputerTool {
    inner: Arc<dyn Tool>,
    agent: String,
    description: String,
}

#[async_trait]
impl Tool for PinnedComputerTool {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn required_capability(&self) -> Option<Capability> {
        self.inner.required_capability()
    }

    fn parameters_schema(&self) -> Json {
        let mut schema = self.inner.parameters_schema();
        if let Some(properties) = schema.get_mut("properties").and_then(Json::as_object_mut) {
            properties.remove("agent");
        }
        if let Some(required) = schema.get_mut("required").and_then(Json::as_array_mut) {
            required.retain(|value| value.as_str() != Some("agent"));
        }
        schema
    }

    async fn invoke(&self, mut args: Json, ctx: &ToolContext) -> Result<Json> {
        let object = args
            .as_object_mut()
            .ok_or_else(|| Error::invalid("computer tool arguments must be an object"))?;
        object.insert("agent".to_string(), Json::String(self.agent.clone()));
        self.inner.invoke(args, ctx).await
    }
}

/// Build the exact registry exposed to `computer_subagent`: selected direct
/// computer controls, pinned to one host, and one parent-defined upstream bridge.
fn restricted_computer_subagent_registry(
    direct: &ToolRegistry,
    agent: &str,
    upstream: Arc<dyn Tool>,
) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    for name in SUBAGENT_COMPUTER_TOOLS {
        if let Some(inner) = direct.get(name) {
            let description = format!(
                "{} This subagent-scoped instance is pinned to `{agent}`; no `agent` argument is \
                 accepted.",
                inner.description()
            );
            registry.register(Arc::new(PinnedComputerTool {
                inner,
                agent: agent.to_string(),
                description,
            }));
        }
    }
    registry.register(upstream);
    registry
}

// ---------------------------------------------------------------------------
// Shared resolution + dispatch
// ---------------------------------------------------------------------------

/// The online machine a tool call targets, resolved from the `agent` argument (or
/// the sole online agent when omitted).
struct Machine {
    id: ComputerAgentId,
    name: String,
    caps: ComputerCapabilities,
}

/// Resolve the target machine: the online agent named/id'd by `arg`, or — when
/// `arg` is absent — the single online agent (ambiguous when several are online).
async fn resolve_machine(
    computer: &ComputerRegistry,
    ws: WorkspaceId,
    arg: Option<String>,
) -> Result<Machine> {
    let online = computer.online_in_workspace(ws).await;
    if online.is_empty() {
        return Err(Error::invalid(
            "no computer agent is online in this workspace — enroll one and start its daemon, \
             then retry (use computer_list_agents to check)",
        ));
    }
    let chosen = match arg {
        Some(a) => {
            let a = a.trim();
            online
                .into_iter()
                .find(|o| o.id.to_string() == a || o.name.eq_ignore_ascii_case(a))
                .ok_or_else(|| {
                    Error::invalid(format!(
                        "no online computer agent matches `{a}` — call computer_list_agents to \
                         see which machines are online"
                    ))
                })?
        }
        None => {
            if online.len() == 1 {
                online.into_iter().next().expect("len checked")
            } else {
                let names = online
                    .iter()
                    .map(|o| o.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(Error::invalid(format!(
                    "several computer agents are online ({names}); pass `agent` (a name or id) \
                     to choose one"
                )));
            }
        }
    };
    Ok(Machine {
        id: chosen.id,
        name: chosen.name,
        caps: chosen.capabilities,
    })
}

/// Send one op to `id` and unwrap the agent's response: the success `data`, or a
/// model-readable error for an agent-reported failure / offline / timeout.
async fn dispatch(
    computer: &ComputerRegistry,
    id: ComputerAgentId,
    op: ComputerOp,
    timeout: Duration,
) -> Result<Json> {
    match computer.request(id, op, timeout).await {
        Ok(resp) if resp.ok => Ok(resp.data),
        Ok(resp) => {
            Err(Error::invalid(resp.error.unwrap_or_else(|| {
                "the computer agent reported a failure".to_string()
            })))
        }
        Err(e) => Err(Error::invalid(format!("computer agent: {e}"))),
    }
}

/// The standard optional `agent` selector, added to each tool's schema.
fn agent_arg_schema() -> Json {
    json!({
        "type": "string",
        "description": "Which computer agent to target, by name or id. Optional when exactly one \
                        agent is online; required to disambiguate when several are."
    })
}

/// Optional working directory shared by filesystem/search/exec computer tools.
fn cwd_arg_schema() -> Json {
    json!({
        "type": "string",
        "description": "Absolute working directory on the machine (optional; must be served). Relative path inputs resolve from it."
    })
}

// ---------------------------------------------------------------------------
// Durable approval gate (shared by exec + request_access)
// ---------------------------------------------------------------------------

/// Outcome of asking the human to approve a sensitive computer action.
enum Approval {
    /// The user already approved this exact call — proceed.
    Approved,
    /// The user rejected it, or there is no one to ask — refuse with this message.
    Refused(String),
    /// A durable approval was recorded and the turn should end — return this marker
    /// (the ws layer renders the Approve/Reject prompt and re-runs on approve).
    Awaiting(Json),
}

/// The require-approval path, reusing the durable, restart-proof
/// [`PendingApproval`](catalerum_core::model::PendingApproval) mechanism (SOUL
/// §7/§12/§19) — identical to the profile tool guard, so a decision is
/// server-enforced and the model can never self-approve. Keyed on
/// `(conversation, tool, arguments)`; a run with no conversation fails closed.
async fn approval_gate(
    store: &Store,
    ctx: &ToolContext,
    tool: &str,
    args: &Json,
    reason: &str,
) -> Approval {
    let (Some(ws), Some(conv)) = (ctx.workspace_id, ctx.conversation_id) else {
        return Approval::Refused(format!(
            "this needs your approval, but there is no interactive conversation to ask in ({reason})"
        ));
    };
    let pending = store.pending_approvals();

    // Resume: did the user already decide this exact call? (Consumes the record.)
    match pending.take_resolved(ws, conv, tool, args).await {
        Ok(Some(ApprovalDecision::Approved)) => return Approval::Approved,
        Ok(Some(ApprovalDecision::Rejected)) => {
            return Approval::Refused(format!("you rejected this ({reason})"))
        }
        Ok(None) => {}
        Err(e) => tracing::warn!(error = %e, "computer approval: reading decision"),
    }

    // Defer: reuse an unresolved record for THIS exact call, else record a new one.
    let id = match pending.get_unresolved(ws, conv).await {
        Ok(Some(existing)) if existing.tool == tool && existing.arguments == *args => existing.id,
        Ok(Some(_)) => {
            return Approval::Refused(
                "another tool call is already awaiting the user's approval; end your turn".into(),
            )
        }
        _ => match pending.create(ws, conv, tool, args, reason).await {
            Ok(p) => p.id,
            Err(e) => {
                tracing::error!(error = %e, "computer approval: recording pending approval");
                return Approval::Refused(format!(
                    "could not record the approval request ({reason})"
                ));
            }
        },
    };

    Approval::Awaiting(json!({
        "status": "awaiting_approval",
        "pending_approval_id": id.to_string(),
        "tool": tool,
        "arguments": args,
        "reason": reason,
        "note": "This action requires the user's approval and has been queued. STOP and end your \
                 turn now — do not call more tools or answer on the user's behalf. It will run once \
                 the user approves.",
    }))
}

// ---------------------------------------------------------------------------
// computer_list_agents
// ---------------------------------------------------------------------------

/// `computer_list_agents` — enumerate the workspace's computer agents (SOUL §20).
pub(crate) struct ComputerListAgentsTool {
    store: Store,
    computer: Arc<ComputerRegistry>,
}

#[async_trait]
impl Tool for ComputerListAgentsTool {
    fn name(&self) -> &str {
        "computer_list_agents"
    }

    fn description(&self) -> &str {
        "List this workspace's enrolled computer agents (installed daemons on servers/desktops) \
         and which are online right now, with each machine's platform, served directories, exec \
         policy, and whether desktop control is enabled. Call this first to see what you can drive \
         and which `agent` name to pass to the other computer_* tools."
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Read, "computer")
    }

    fn parameters_schema(&self) -> Json {
        json!({ "type": "object", "properties": {} })
    }

    async fn invoke(&self, _args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let agents = self.store.computer_agents().list_by_workspace(ws).await?;
        let online = self.computer.online_in_workspace(ws).await;
        let online_ids: std::collections::HashSet<ComputerAgentId> =
            online.iter().map(|o| o.id).collect();

        let list: Vec<Json> = agents
            .into_iter()
            .filter(|a| a.is_active())
            .map(|a| {
                let is_online = online_ids.contains(&a.id);
                // Prefer live capabilities for an online agent; else the stored snapshot.
                let caps = online
                    .iter()
                    .find(|o| o.id == a.id)
                    .map(|o| o.capabilities.clone())
                    .or(a.capabilities);
                let caps_json = caps.map(|c| {
                    json!({
                        "platform": c.platform.label(),
                        "hostname": c.hostname,
                        "os": c.os,
                        "dirs": c.dirs.iter().map(|d| json!({
                            "path": d.path,
                            "mode": if d.mode.can_write() { "read_write" } else { "read" },
                        })).collect::<Vec<_>>(),
                        "grantable_roots": c.grantable_roots,
                        "exec_policy": exec_policy_label(&c),
                        "desktop": c.desktop,
                        "sandbox": c.sandbox.label(),
                    })
                });
                json!({
                    "id": a.id,
                    "name": a.name,
                    "online": is_online,
                    "last_seen_at": a.last_seen_at,
                    "capabilities": caps_json,
                })
            })
            .collect();
        Ok(json!({ "agents": list, "online_count": online.len() }))
    }
}

/// The machine's exec policy as a wire token, for display.
fn exec_policy_label(c: &ComputerCapabilities) -> String {
    serde_json::to_value(c.exec_policy)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "auto".to_string())
}

// ---------------------------------------------------------------------------
// computer_list_dir
// ---------------------------------------------------------------------------

pub(crate) struct ComputerListDirTool {
    computer: Arc<ComputerRegistry>,
}

#[async_trait]
impl Tool for ComputerListDirTool {
    fn name(&self) -> &str {
        "computer_list_dir"
    }

    fn description(&self) -> &str {
        "List the entries of a directory on a computer agent's machine. The path must be inside \
         one of the machine's served directories (see computer_list_agents). Returns each entry's \
         name, absolute path, kind (file/dir/symlink), and size. Pass `cwd` to resolve a relative \
         `path`."
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Read, "computer")
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "agent": agent_arg_schema(),
                "cwd": cwd_arg_schema(),
                "path": { "type": "string", "description": "Directory path on the machine; absolute or relative to `cwd`." }
            },
            "required": ["path"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let machine = resolve_machine(&self.computer, ws, opt_str_some(&args, "agent")).await?;
        let cwd = opt_str_some(&args, "cwd");
        let path = required_str(&args, "path")?;
        dispatch(
            &self.computer,
            machine.id,
            ComputerOp::ListDir { cwd, path },
            DEFAULT_OP_TIMEOUT,
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// computer_read_file
// ---------------------------------------------------------------------------

pub(crate) struct ComputerReadFileTool {
    computer: Arc<ComputerRegistry>,
}

#[async_trait]
impl Tool for ComputerReadFileTool {
    fn name(&self) -> &str {
        "computer_read_file"
    }

    fn description(&self) -> &str {
        "Read a UTF-8 text file on a computer agent's machine (optionally a byte window via \
         `offset`/`limit`). The path must be inside a served directory. Large files are truncated \
         with `truncated: true`. Pass `cwd` to resolve a relative `path`."
    }

    fn description_for(&self, input_modalities: &[String]) -> String {
        computer_read_file_description(self.description(), input_modalities)
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Read, "computer")
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "agent": agent_arg_schema(),
                "cwd": cwd_arg_schema(),
                "path": { "type": "string", "description": "File path on the machine; absolute or relative to `cwd`." },
                "offset": { "type": "integer", "description": "Byte offset to start reading from (optional)." },
                "limit": { "type": "integer", "description": "Maximum bytes to read (optional)." }
            },
            "required": ["path"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        self.invoke_for_model(args, ctx, &[]).await
    }

    async fn invoke_for_model(
        &self,
        args: Json,
        ctx: &ToolContext,
        input_modalities: &[String],
    ) -> Result<Json> {
        let ws = workspace(ctx)?;
        let machine = resolve_machine(&self.computer, ws, opt_str_some(&args, "agent")).await?;
        let cwd = opt_str_some(&args, "cwd");
        let path = required_str(&args, "path")?;
        let offset = args.get("offset").and_then(Json::as_u64);
        let limit = args.get("limit").and_then(Json::as_u64);
        let media = computer_supported_media_path(&path, input_modalities);
        if media.is_some() && (offset.is_some() || limit.is_some()) {
            return Err(Error::invalid(
                "`offset`/`limit` cannot be used when ingesting binary media",
            ));
        }
        let mut result = dispatch(
            &self.computer,
            machine.id,
            ComputerOp::ReadFile {
                cwd,
                path: path.clone(),
                offset,
                limit,
                media_content_type: media.map(str::to_string),
            },
            DEFAULT_OP_TIMEOUT,
        )
        .await?;
        let Some(content_type) = media else {
            return Ok(result);
        };
        let encoded = result
            .as_object_mut()
            .and_then(|object| object.remove("content_base64"))
            .and_then(|value| value.as_str().map(str::to_string))
            .ok_or_else(|| Error::provider("computer agent returned malformed media data"))?;
        let input = MediaInput::Image {
            url: format!("data:{content_type};base64,{encoded}"),
        };
        let object = result
            .as_object_mut()
            .ok_or_else(|| Error::provider("computer agent returned malformed media metadata"))?;
        object.insert("ingested".to_string(), Json::Bool(true));
        object.insert(
            MODEL_MEDIA_RESULT_FIELD.to_string(),
            serde_json::to_value([input]).expect("MediaInput serialization cannot fail"),
        );
        Ok(result)
    }
}

fn computer_supported_media_path(path: &str, input_modalities: &[String]) -> Option<&'static str> {
    let content_type = mime_guess::from_path(path).first_raw()?;
    (content_type.starts_with("image/")
        && input_modalities
            .iter()
            .any(|modality| modality.eq_ignore_ascii_case("image")))
    .then_some(content_type)
}

fn computer_read_file_description(base: &str, input_modalities: &[String]) -> String {
    let supports_images = input_modalities
        .iter()
        .any(|modality| modality.eq_ignore_ascii_case("image"));
    if !supports_images {
        return format!(
            "{base} Binary files are rejected; native binary ingestion is unavailable for the \
             active model through llmleaf."
        );
    }
    format!(
        "{base} Binary files are rejected by default. Because the active model accepts image input, \
         recognized image files are ingested natively through llmleaf and attached to the next \
         model turn; do not use `offset`/`limit` for images.",
    )
}

// ---------------------------------------------------------------------------
// computer_stat
// ---------------------------------------------------------------------------

pub(crate) struct ComputerStatTool {
    computer: Arc<ComputerRegistry>,
}

#[async_trait]
impl Tool for ComputerStatTool {
    fn name(&self) -> &str {
        "computer_stat"
    }

    fn description(&self) -> &str {
        "Check whether a path exists on a computer agent's machine and its kind/size/modified time. \
         The path must be inside a served directory. Pass `cwd` to resolve a relative `path`."
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Read, "computer")
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "agent": agent_arg_schema(),
                "cwd": cwd_arg_schema(),
                "path": { "type": "string", "description": "Path on the machine; absolute or relative to `cwd`." }
            },
            "required": ["path"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let machine = resolve_machine(&self.computer, ws, opt_str_some(&args, "agent")).await?;
        let cwd = opt_str_some(&args, "cwd");
        let path = required_str(&args, "path")?;
        dispatch(
            &self.computer,
            machine.id,
            ComputerOp::Stat { cwd, path },
            DEFAULT_OP_TIMEOUT,
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// computer_search
// ---------------------------------------------------------------------------

pub(crate) struct ComputerSearchTool {
    computer: Arc<ComputerRegistry>,
}

fn effective_search_timeout_secs(args: &Json) -> u64 {
    args.get("timeout_secs")
        .and_then(Json::as_u64)
        .unwrap_or(DEFAULT_SEARCH_TIMEOUT_SECS)
        .clamp(1, MAX_SEARCH_TIMEOUT_SECS)
}

#[async_trait]
impl Tool for ComputerSearchTool {
    fn name(&self) -> &str {
        "computer_search"
    }

    fn description(&self) -> &str {
        "Broad recursive search under a computer agent's served directories: matches file and \
         directory *names* as well as file *contents* against a string (case-insensitive) or a \
         regular expression when `regex` is true. Each match has `kind` `name` or `content` \
         (content matches include the line number and matched line). Hidden (dot-prefixed) files \
         and directories are skipped unless `include_hidden` is true. The search returns matches \
         accumulated within `timeout_secs` (default 10 seconds). Pass `cwd` to search from a \
         working directory or to resolve a relative `root`."
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Read, "computer")
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "agent": agent_arg_schema(),
                "query": { "type": "string", "description": "Text or regex to search for." },
                "cwd": cwd_arg_schema(),
                "root": { "type": "string", "description": "Directory to search under (optional; absolute or relative to `cwd`; defaults to `cwd`, then all served directories)." },
                "regex": { "type": "boolean", "description": "Treat `query` as a regular expression (default false)." },
                "max_results": { "type": "integer", "description": "Cap on the number of matches (optional)." },
                "include_hidden": { "type": "boolean", "description": "Also search hidden (dot-prefixed) files and directories (default false)." },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Wall-clock search budget in seconds; returns matches found by the deadline (default 10, max 3600).",
                    "minimum": 1,
                    "maximum": MAX_SEARCH_TIMEOUT_SECS,
                    "default": DEFAULT_SEARCH_TIMEOUT_SECS
                }
            },
            "required": ["query"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let machine = resolve_machine(&self.computer, ws, opt_str_some(&args, "agent")).await?;
        let query = required_str(&args, "query")?;
        let cwd = opt_str_some(&args, "cwd");
        let root = opt_str_some(&args, "root");
        let regex = args.get("regex").and_then(Json::as_bool).unwrap_or(false);
        let max_results = args.get("max_results").and_then(Json::as_u64);
        let include_hidden = args
            .get("include_hidden")
            .and_then(Json::as_bool)
            .unwrap_or(false);
        let timeout_secs = effective_search_timeout_secs(&args);
        dispatch(
            &self.computer,
            machine.id,
            ComputerOp::Search {
                cwd,
                root,
                query,
                regex,
                max_results,
                include_hidden,
                timeout_secs: Some(timeout_secs),
            },
            Duration::from_secs(timeout_secs + 5),
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// computer_write_file
// ---------------------------------------------------------------------------

pub(crate) struct ComputerWriteFileTool {
    computer: Arc<ComputerRegistry>,
}

#[async_trait]
impl Tool for ComputerWriteFileTool {
    fn name(&self) -> &str {
        "computer_write_file"
    }

    fn description(&self) -> &str {
        "Write a text file on a computer agent's machine. The path must be inside a directory the \
         machine serves **read-write**. `mode` is `overwrite` (default), `create_new` (fail if it \
         exists), or `append`. Pass `cwd` to resolve a relative `path`."
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "computer")
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "agent": agent_arg_schema(),
                "cwd": cwd_arg_schema(),
                "path": { "type": "string", "description": "File path on the machine; absolute or relative to `cwd` and under a read-write directory." },
                "content": { "type": "string", "description": "The full text to write." },
                "mode": {
                    "type": "string",
                    "enum": ["overwrite", "create_new", "append"],
                    "description": "How to write: overwrite (default), create_new, or append."
                }
            },
            "required": ["path", "content"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let machine = resolve_machine(&self.computer, ws, opt_str_some(&args, "agent")).await?;
        let cwd = opt_str_some(&args, "cwd");
        let path = required_str(&args, "path")?;
        let content = args
            .get("content")
            .and_then(Json::as_str)
            .ok_or_else(|| Error::invalid("`content` is required"))?
            .to_string();
        let mode = match opt_str_some(&args, "mode").as_deref() {
            Some("create_new") => WriteMode::CreateNew,
            Some("append") => WriteMode::Append,
            _ => WriteMode::Overwrite,
        };
        dispatch(
            &self.computer,
            machine.id,
            ComputerOp::WriteFile {
                cwd,
                path,
                content,
                mode,
            },
            DEFAULT_OP_TIMEOUT,
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// computer_request_access
// ---------------------------------------------------------------------------

pub(crate) struct ComputerRequestAccessTool {
    store: Store,
    computer: Arc<ComputerRegistry>,
}

#[async_trait]
impl Tool for ComputerRequestAccessTool {
    fn name(&self) -> &str {
        "computer_request_access"
    }

    fn description(&self) -> &str {
        "Request that a computer agent grant access to an additional directory (read or read-write) \
         for the rest of this session. The directory must be under one of the machine's advertised \
         `grantable_roots`. This ALWAYS requires the user's approval; once approved, the machine \
         serves that directory too. Pass `cwd` to resolve a relative `path`."
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "computer")
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "agent": agent_arg_schema(),
                "cwd": cwd_arg_schema(),
                "path": { "type": "string", "description": "Directory path to request; absolute or relative to `cwd`, and under a grantable root." },
                "mode": {
                    "type": "string",
                    "enum": ["read", "read_write"],
                    "description": "Requested access level (default read)."
                }
            },
            "required": ["path"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let machine = resolve_machine(&self.computer, ws, opt_str_some(&args, "agent")).await?;
        let cwd = opt_str_some(&args, "cwd");
        let path = required_str(&args, "path")?;
        let mode = match opt_str_some(&args, "mode").as_deref() {
            Some("read_write") => DirMode::ReadWrite,
            _ => DirMode::Read,
        };

        // Granting a new directory on a host is always sensitive — always ask.
        let target = cwd.as_ref().map_or_else(
            || format!("`{path}`"),
            |cwd| format!("`{path}` from cwd `{cwd}`"),
        );
        let reason = format!(
            "grant {} access to {target} on {}",
            if mode.can_write() {
                "read-write"
            } else {
                "read"
            },
            machine.name
        );
        match approval_gate(&self.store, ctx, self.name(), &args, &reason).await {
            Approval::Approved => {}
            Approval::Refused(msg) => return Err(Error::invalid(msg)),
            Approval::Awaiting(marker) => return Ok(marker),
        }

        dispatch(
            &self.computer,
            machine.id,
            ComputerOp::GrantAccess { cwd, path, mode },
            DEFAULT_OP_TIMEOUT,
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// computer_exec  (the "auto mode" classifier + approval)
// ---------------------------------------------------------------------------

pub(crate) struct ComputerExecTool {
    store: Store,
    client: OpenRouterClient,
    computer: Arc<ComputerRegistry>,
    default_model: String,
}

/// A command-safety ruling.
enum ExecRuling {
    Allow,
    Deny(String),
    Ask(String),
}

impl ComputerExecTool {
    /// The one-shot LLM command-safety classifier for `auto` mode. Fails **closed**
    /// (an unavailable/garbled classifier → `ask`, never `allow`).
    async fn classify(&self, machine: &str, command: &str) -> ExecRuling {
        let system = "You are a command-safety classifier for an AI agent about to run a shell \
             command on a user's own computer. Judge the command's RISK and reply with a compact \
             JSON object {\"decision\":\"allow\"|\"deny\"|\"ask\",\"reason\":\"…\"}. \
             \"allow\" = clearly safe: read-only inspection, builds/tests, routine dev commands \
             scoped to a project. \"ask\" = a human should confirm: deleting/moving files, \
             installing software, editing system config, anything writing broadly or reaching the \
             network in an unusual way, or anything ambiguous. \"deny\" = obviously catastrophic or \
             malicious: wiping disks (e.g. rm -rf /), fork bombs, exfiltrating credentials, opening \
             reverse shells, disabling security. When unsure, prefer \"ask\".";
        let user = format!("Machine: {machine}\nCommand:\n{command}");
        let req = ChatRequest::new(
            self.default_model.clone(),
            vec![ChatMessage::system(system), ChatMessage::user(user)],
        );
        match self.client.chat(req).await {
            Ok(turn) => parse_exec_ruling(&turn.content),
            Err(e) => {
                tracing::warn!(error = %e, "computer_exec: classifier unavailable — failing closed to ask");
                ExecRuling::Ask("the command-safety classifier is unavailable".into())
            }
        }
    }
}

#[async_trait]
impl Tool for ComputerExecTool {
    fn name(&self) -> &str {
        "computer_exec"
    }

    fn description(&self) -> &str {
        "Run a shell command on a computer agent's machine and return its stdout, stderr, and exit \
         code. Commands are gated by the machine's exec policy: `always_allow` runs it, `deny` \
         refuses, `always_ask` needs your approval, and `auto` (the default) runs a safety \
         classifier that allows safe commands, blocks catastrophic ones, and asks you to confirm \
         risky ones. When approval is needed the call is queued — stop and let the user decide."
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Run, "computer")
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "agent": agent_arg_schema(),
                "command": { "type": "string", "description": "The shell command line to run." },
                "cwd": cwd_arg_schema(),
                "timeout_secs": { "type": "integer", "description": "Kill the command after this many seconds (optional; default 300, max 3600)." }
            },
            "required": ["command"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        use catalerum_core::computer::ExecPolicy;
        let ws = workspace(ctx)?;
        let machine = resolve_machine(&self.computer, ws, opt_str_some(&args, "agent")).await?;
        let command = required_str(&args, "command")?;
        let cwd = opt_str_some(&args, "cwd");
        let timeout_secs = args.get("timeout_secs").and_then(Json::as_u64);

        // Decide whether the command runs, is blocked, or needs the user's approval.
        let ruling = match machine.caps.exec_policy {
            ExecPolicy::Deny => {
                return Err(Error::invalid(format!(
                    "command execution is disabled on {} (its exec policy is `deny`)",
                    machine.name
                )))
            }
            ExecPolicy::AlwaysAllow => ExecRuling::Allow,
            ExecPolicy::AlwaysAsk => {
                ExecRuling::Ask("this machine requires approval for every command".into())
            }
            ExecPolicy::Auto => self.classify(&machine.name, &command).await,
        };

        let ask_reason = match ruling {
            ExecRuling::Allow => None,
            ExecRuling::Deny(reason) => {
                return Err(Error::invalid(format!(
                    "the command was blocked by the safety classifier: {reason}"
                )))
            }
            ExecRuling::Ask(reason) => Some(reason),
        };

        if let Some(reason) = ask_reason {
            let reason = format!("run `{command}` on {} ({reason})", machine.name);
            match approval_gate(&self.store, ctx, self.name(), &args, &reason).await {
                Approval::Approved => {}
                Approval::Refused(msg) => return Err(Error::invalid(msg)),
                Approval::Awaiting(marker) => return Ok(marker),
            }
        }

        // Bound the wait on the agent by the command's effective timeout plus a
        // margin, so the agent's own `timed_out` result arrives before we give up.
        let wait = Duration::from_secs(
            timeout_secs
                .unwrap_or(DEFAULT_EXEC_TIMEOUT_SECS)
                .min(MAX_EXEC_TIMEOUT_SECS)
                + 15,
        );
        dispatch(
            &self.computer,
            machine.id,
            ComputerOp::Exec {
                command,
                cwd,
                timeout_secs,
                stdin: None,
            },
            wait,
        )
        .await
    }
}

/// Parse a classifier reply into a ruling. Prefers a JSON object anywhere in the
/// text, else scans for a keyword; anything unrecognised → `ask` (fail closed).
fn parse_exec_ruling(text: &str) -> ExecRuling {
    // Try a JSON object first.
    if let (Some(s), Some(e)) = (text.find('{'), text.rfind('}')) {
        if e > s {
            if let Ok(v) = serde_json::from_str::<Json>(&text[s..=e]) {
                let decision = v.get("decision").and_then(Json::as_str).unwrap_or("");
                let reason = v
                    .get("reason")
                    .and_then(Json::as_str)
                    .unwrap_or("")
                    .to_string();
                if let Some(r) = ruling_from_word(decision, &reason) {
                    return r;
                }
            }
        }
    }
    ruling_from_word(text, "")
        .unwrap_or_else(|| ExecRuling::Ask("the command's safety could not be determined".into()))
}

/// Map a decision word (tolerating a surrounding sentence) to a ruling.
fn ruling_from_word(raw: &str, reason: &str) -> Option<ExecRuling> {
    let s = raw.trim().to_ascii_lowercase();
    let has = |n: &str| s == n || s.contains(n);
    let reason = if reason.trim().is_empty() {
        None
    } else {
        Some(reason.trim().to_string())
    };
    if has("deny") || has("block") || has("refuse") || has("forbid") {
        Some(ExecRuling::Deny(
            reason.unwrap_or_else(|| "the command is dangerous".into()),
        ))
    } else if has("ask") || has("confirm") || has("feedback") {
        Some(ExecRuling::Ask(
            reason.unwrap_or_else(|| "the command should be confirmed".into()),
        ))
    } else if has("allow") || has("permit") || s == "ok" || has("safe") {
        Some(ExecRuling::Allow)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// computer_desktop
// ---------------------------------------------------------------------------

pub(crate) struct ComputerDesktopTool {
    computer: Arc<ComputerRegistry>,
}

#[async_trait]
impl Tool for ComputerDesktopTool {
    fn name(&self) -> &str {
        "computer_desktop"
    }

    fn description(&self) -> &str {
        "Perform a desktop action on a computer agent's machine (only when the machine advertises \
         desktop control): `screenshot` (returns a base64 PNG of the primary screen), `open_url` \
         (open a URL in the default browser), or `notify` (show a desktop notification)."
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Run, "computer")
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "agent": agent_arg_schema(),
                "action": {
                    "type": "string",
                    "enum": ["screenshot", "open_url", "notify"],
                    "description": "The desktop action to perform."
                },
                "url": { "type": "string", "description": "URL for `open_url`." },
                "title": { "type": "string", "description": "Title for `notify`." },
                "body": { "type": "string", "description": "Body for `notify`." }
            },
            "required": ["action"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let machine = resolve_machine(&self.computer, ws, opt_str_some(&args, "agent")).await?;
        if !machine.caps.desktop {
            return Err(Error::invalid(format!(
                "{} does not have desktop control enabled",
                machine.name
            )));
        }
        let action = match required_str(&args, "action")?.as_str() {
            "screenshot" => DesktopAction::Screenshot,
            "open_url" => DesktopAction::OpenUrl {
                url: required_str(&args, "url")?,
            },
            "notify" => DesktopAction::Notify {
                title: opt_str(&args, "title"),
                body: opt_str(&args, "body"),
            },
            other => {
                return Err(Error::invalid(format!(
                    "unknown desktop action `{other}` (expected screenshot / open_url / notify)"
                )))
            }
        };
        dispatch(
            &self.computer,
            machine.id,
            ComputerOp::Desktop { action },
            DEFAULT_OP_TIMEOUT,
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// computer_agent_task  (delegate-style: run a subagent that drives one machine)
// ---------------------------------------------------------------------------

pub(crate) struct ComputerAgentTaskTool {
    store: Store,
    client: OpenRouterClient,
    computer: Arc<ComputerRegistry>,
    default_model: String,
}

#[async_trait]
impl Tool for ComputerAgentTaskTool {
    fn name(&self) -> &str {
        "computer_agent_task"
    }

    fn description(&self) -> &str {
        "Hand a whole natural-language task to one computer agent: a subagent runs in its own \
         fresh context with only that machine's file/search/exec/desktop tools, works the task \
         end-to-end on the machine, and returns a concise result — keeping your own context lean. \
         Use this for multi-step machine work (e.g. 'find and fix the failing test in ~/proj'); use \
         the direct computer_* tools for a single operation. The subagent runs under your authority \
         and cannot exceed it. Pass `profile` to use a named agent profile's model, instructions, \
         skills, tool restrictions, guard, and attenuated grant. Pass `cwd` to give the subagent \
         a default working directory."
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Run, "computer")
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "agent": agent_arg_schema(),
                "cwd": cwd_arg_schema(),
                "profile": {
                    "type": "string",
                    "description": "Optional named agent profile to configure and scope the subagent instead of using the workspace default model."
                },
                "task": { "type": "string", "description": "The task to accomplish on the machine, in plain language." }
            },
            "required": ["task"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let profile =
            selected_subagent_profile(&self.store, &self.default_model, &args, ctx, ws).await?;
        let machine = resolve_machine(&self.computer, ws, opt_str_some(&args, "agent")).await?;
        let cwd = opt_str_some(&args, "cwd");
        let task = required_str(&args, "task")?;

        let dirs = machine
            .caps
            .dirs
            .iter()
            .map(|d| {
                format!(
                    "{} ({})",
                    d.path,
                    if d.mode.can_write() { "rw" } else { "ro" }
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let cwd_instruction = cwd.map_or_else(String::new, |cwd| {
            format!(
                " Default working directory: `{cwd}`. Pass cwd=`{cwd}` to filesystem, search, and \
                 exec tools unless the task explicitly requires another directory."
            )
        });
        let assignment = format!(
            "You control a computer named \"{name}\" ({platform}). Use the computer_* tools to work \
             on THAT machine — always pass agent=\"{name}\". Served directories: {dirs}. Exec policy: \
             {policy}.{cwd_instruction} Do the user's task, then reply with a concise summary of what you did and the \
             key results. Do not ask for confirmation you don't need; if a command needs the user's \
             approval and none is available, report that instead of retrying.",
            name = machine.name,
            platform = machine.caps.platform.label(),
            dirs = if dirs.is_empty() { "(none advertised)".into() } else { dirs },
            policy = exec_policy_label(&machine.caps),
        );
        let system = profile.as_ref().map_or_else(
            || assignment.clone(),
            |profile| {
                format!(
                    "{}\n\n# Constrained computer assignment\n\n{assignment}",
                    profile.system
                )
            },
        );

        // The subagent runs under the caller's own authority (⊆ the caller), acting
        // as the caller so approvals surface on the caller's conversation.
        let sub_ctx = ToolContext {
            workspace_id: Some(ws),
            user_id: ctx.user_id,
            agent_id: ctx.agent_id,
            grant_id: profile.as_ref().and_then(|profile| profile.grant_id),
            capabilities: profile
                .as_ref()
                .map(|profile| profile.capabilities.clone())
                .or_else(|| ctx.capabilities.clone()),
            dry_run: ctx.dry_run || profile.as_ref().is_some_and(|profile| profile.dry_run),
            gate: None,
            conversation_id: ctx.conversation_id,
            ui_id: None,
            registry: None,
        };
        let sub_registry = direct_computer_registry(
            &self.store,
            &self.client,
            &self.computer,
            &self.default_model,
        );
        let model = profile.as_ref().map_or_else(
            || self.default_model.clone(),
            |profile| profile.model.clone(),
        );
        let mut sub_ctx = sub_ctx;
        sub_ctx.gate = profile.as_ref().and_then(|profile| {
            crate::tool_gate::build_gate(
                profile.guard.as_ref(),
                sub_registry.clone(),
                self.store.clone(),
                sub_ctx.clone(),
                self.client.clone(),
                model.clone(),
            )
        });
        let agent_config = AgentConfig {
            cost_limit: profile.as_ref().and_then(|profile| profile.cost_limit),
            ..AgentConfig::default()
        };
        let outcome = run_agent(
            &self.client,
            ChatRequest::new(
                model,
                vec![ChatMessage::system(system), ChatMessage::user(task)],
            ),
            &sub_registry,
            &sub_ctx,
            &agent_config,
            profile
                .as_ref()
                .and_then(|profile| profile.allowed_tools.as_deref()),
        )
        .await
        .map_err(|e| Error::invalid(format!("computer subagent loop failed: {e}")))?;

        Ok(json!({
            "machine": machine.name,
            "profile": profile.as_ref().map(|profile| profile.name.as_str()),
            "content": outcome.content,
            "tool_calls": outcome.tool_invocations.len(),
        }))
    }
}

// ---------------------------------------------------------------------------
// computer_subagent  (machine-only worker + parent-defined upstream boundary)
// ---------------------------------------------------------------------------

const UPSTREAM_TOOL: &str = "upstream";
const MAX_UPSTREAM_CODE_BYTES: usize = 64 * 1024;

/// The only host bridge available to the parent-authored Boa interaction layer.
/// Calls re-enter the parent's exact registry and context, so its capabilities,
/// programmable gate, dry-run posture, and conversation approval path remain in
/// force. Nested Boa and computer-subagent calls are refused to keep this bridge
/// one level deep and prevent an indirect recursion tunnel.
struct UpstreamHost {
    registry: ToolRegistry,
    parent_ctx: ToolContext,
    cancel: CancellationToken,
}

impl catalerum_script::UiScriptHost for UpstreamHost {
    fn call_tool(&self, tool: &str, args: Json) -> std::result::Result<Json, String> {
        if self.cancel.is_cancelled() {
            return Err("computer subagent was stopped".into());
        }
        if matches!(
            tool,
            "run_javascript" | "computer_subagent" | "computer_agent_task"
        ) {
            return Err(format!(
                "upstream handler cannot call `{tool}` — nested Boa/subagent execution is disabled"
            ));
        }
        tokio::runtime::Handle::current()
            .block_on(self.registry.dispatch(tool, args, &self.parent_ctx))
            .map_err(|e| e.to_string())
    }
}

/// A dynamically configured tool installed only in one computer subagent's
/// private registry. The child sees the prose contract and a single JSON request
/// argument, but never sees the handler source, its context, or any parent tool
/// specs. The Boa body receives `{ request, context }` as `input`.
struct UpstreamTool {
    description: String,
    source: String,
    context: Json,
    runner: Arc<ScriptCodeRunner>,
    parent_registry: Option<ToolRegistry>,
    parent_ctx: ToolContext,
    cancel: CancellationToken,
}

#[async_trait]
impl Tool for UpstreamTool {
    fn name(&self) -> &str {
        UPSTREAM_TOOL
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "request": {
                    "description": "A focused JSON request to the parent-defined interaction layer. Its shape depends on this tool's description."
                }
            },
            "required": ["request"]
        })
    }

    async fn invoke(&self, args: Json, _ctx: &ToolContext) -> Result<Json> {
        if self.cancel.is_cancelled() {
            return Err(Error::other("computer subagent was stopped"));
        }
        let request = args
            .get("request")
            .cloned()
            .ok_or_else(|| Error::invalid("upstream requires `request`"))?;
        let input = json!({
            "request": request,
            "context": self.context,
        });
        // A direct Tool::invoke (principally unit/embedded use) has no dispatching
        // parent registry and therefore remains a pure Boa transform. A real chat
        // dispatch injects the exact per-run registry. Emerged-UI contexts also
        // stay pure: their handler-tool allow-list must not be tunnelled through.
        let result = match (&self.parent_registry, self.parent_ctx.ui_id) {
            (Some(registry), None) => {
                let host = Arc::new(UpstreamHost {
                    registry: registry.clone(),
                    parent_ctx: self.parent_ctx.clone(),
                    cancel: self.cancel.clone(),
                });
                self.runner.eval_with_host(&self.source, &input, host).await
            }
            _ => self.runner.eval_pure(&self.source, &input).await,
        }
        .map_err(|e| Error::invalid(format!("upstream handler failed: {e}")))?;
        Ok(json!({ "response": result }))
    }
}

#[derive(Clone)]
pub(crate) struct ComputerSubagentTool {
    store: Store,
    client: OpenRouterClient,
    computer: Arc<ComputerRegistry>,
    default_model: String,
    subagent_runs: SubagentRunManager,
    /// Set only on the private tool clone owned by a background run.
    run_cancel: Option<CancellationToken>,
}

#[async_trait]
impl Tool for ComputerSubagentTool {
    fn name(&self) -> &str {
        "computer_subagent"
    }

    fn description(&self) -> &str {
        "Spawn a focused subagent for a multi-step task on one computer. The child receives only \
         direct computer controls pinned to the selected machine and one `upstream` tool. You \
         define `upstream` with a bounded Boa JavaScript function body: it receives \
         `input.request` from the child plus your optional `input.context`, and may selectively \
         call your own tools with `catalerum.callTool(name, args)`. Those calls run through your \
         exact registry, capabilities, policy gate, and dry-run rules; the child never sees or \
         calls your other tools directly. Use `upstream_description` to tell the child what the \
         channel can answer. Pass `profile` to use a named agent profile's model, instructions, \
         skills, tool restrictions, guard, and attenuated grant. The handler must return the \
         response value. Set `background=true` \
         to return a run id immediately; then use monitor_subagent, wait_subagent, or \
         stop_subagent from the parent."
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Run, "computer")
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "agent": agent_arg_schema(),
                "cwd": cwd_arg_schema(),
                "task": {
                    "type": "string",
                    "description": "The self-contained task to accomplish on the selected machine."
                },
                "profile": {
                    "type": "string",
                    "description": "Optional named agent profile to configure and scope the subagent instead of using the workspace default model."
                },
                "upstream_description": {
                    "type": "string",
                    "description": "The contract advertised to the child for its `upstream` tool: what it may ask and the expected request shape."
                },
                "upstream_code": {
                    "type": "string",
                    "description": "Bounded Boa JavaScript function body for the upstream interaction layer. Read `input.request` and `input.context`, optionally call parent tools via `catalerum.callTool(name, args)`, and return only what the child may receive."
                },
                "upstream_context": {
                    "description": "Optional parent-selected JSON made available to the handler as `input.context`; it is never exposed to the child except through the handler's return value."
                },
                "background": {
                    "type": "boolean",
                    "default": false,
                    "description": "Start on this API pod and return a controllable run id immediately."
                }
            },
            "required": ["task", "upstream_code"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        if self.run_cancel.is_none()
            && args
                .get("background")
                .and_then(Json::as_bool)
                .unwrap_or(false)
        {
            let task = required_str(&args, "task")?;
            let label = task.chars().take(160).collect::<String>();
            let mut run_args = args;
            if let Some(object) = run_args.as_object_mut() {
                object.remove("background");
            }
            let template = self.clone();
            let parent_ctx = ctx.clone();
            return self
                .subagent_runs
                .spawn(ctx, "computer_subagent", label, move |cancel| async move {
                    let runner: Arc<dyn Tool> = Arc::new(Self {
                        run_cancel: Some(cancel),
                        ..template
                    });
                    runner.invoke(run_args, &parent_ctx).await
                })
                .await;
        }
        let ws = workspace(ctx)?;
        let profile =
            selected_subagent_profile(&self.store, &self.default_model, &args, ctx, ws).await?;
        let machine = resolve_machine(&self.computer, ws, opt_str_some(&args, "agent")).await?;
        let cwd = opt_str_some(&args, "cwd");
        let task = required_str(&args, "task")?;
        if task.trim().is_empty() {
            return Err(Error::invalid(
                "computer_subagent requires a non-empty `task`",
            ));
        }
        let upstream_code = required_str(&args, "upstream_code")?;
        if upstream_code.trim().is_empty() {
            return Err(Error::invalid(
                "computer_subagent requires non-empty `upstream_code`",
            ));
        }
        if upstream_code.len() > MAX_UPSTREAM_CODE_BYTES {
            return Err(Error::invalid(format!(
                "computer_subagent `upstream_code` exceeds the {MAX_UPSTREAM_CODE_BYTES}-byte limit"
            )));
        }
        let upstream_contract = opt_str_some(&args, "upstream_description").unwrap_or_else(|| {
            "Send a focused JSON request when you need information from the parent.".to_string()
        });
        let upstream_context = args.get("upstream_context").cloned().unwrap_or(Json::Null);
        let cancel = self.run_cancel.clone().unwrap_or_default();

        let dirs = machine
            .caps
            .dirs
            .iter()
            .map(|d| {
                format!(
                    "{} ({})",
                    d.path,
                    if d.mode.can_write() { "rw" } else { "ro" }
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let cwd_instruction = cwd.map_or_else(String::new, |cwd| {
            format!(
                " Default working directory: `{cwd}`. Pass cwd=`{cwd}` to filesystem, search, and \
                 exec tools unless the task explicitly requires another directory."
            )
        });
        let assignment = format!(
            "You are a focused computer subagent controlling exactly one machine: \"{name}\" \
             ({platform}). Your available computer_* tools are already pinned to it; no `agent` \
             argument is needed. Served directories: {dirs}. Exec policy: {policy}.{cwd_instruction} \
             You also have one parent-mediated information channel named `upstream`; it is your \
             only route to non-machine information. Its contract is: {upstream_contract} Ask it \
             focused questions only when needed. Complete the task, then return a concise, \
             self-contained summary of what you changed and the key results. If approval is \
             required and unavailable, report that instead of retrying.",
            name = machine.name,
            platform = machine.caps.platform.label(),
            dirs = if dirs.is_empty() {
                "(none advertised)".into()
            } else {
                dirs
            },
            policy = exec_policy_label(&machine.caps),
        );
        let system = profile.as_ref().map_or_else(
            || assignment.clone(),
            |profile| {
                format!(
                    "{}\n\n# Constrained computer assignment\n\n{assignment}",
                    profile.system
                )
            },
        );

        // The child uses the caller's authority only for its machine controls.
        // `upstream` separately captures the full parent context so every nested
        // parent call is re-checked under the original gate and conversation.
        let mut sub_ctx = ToolContext {
            workspace_id: Some(ws),
            user_id: ctx.user_id,
            agent_id: ctx.agent_id,
            grant_id: profile.as_ref().and_then(|profile| profile.grant_id),
            capabilities: profile
                .as_ref()
                .map(|profile| profile.capabilities.clone())
                .or_else(|| ctx.capabilities.clone()),
            dry_run: ctx.dry_run || profile.as_ref().is_some_and(|profile| profile.dry_run),
            gate: None,
            conversation_id: ctx.conversation_id,
            ui_id: None,
            registry: None,
        };
        let direct = direct_computer_registry(
            &self.store,
            &self.client,
            &self.computer,
            &self.default_model,
        );
        let mut upstream_ctx = ctx.clone();
        if let Some(profile) = &profile {
            upstream_ctx.grant_id = profile.grant_id;
            upstream_ctx.capabilities = Some(profile.capabilities.clone());
            upstream_ctx.dry_run |= profile.dry_run;
        }
        let upstream = Arc::new(UpstreamTool {
            description: format!(
                "Parent-mediated information channel. {upstream_contract} The parent controls \
                 what each request reveals."
            ),
            source: upstream_code,
            context: upstream_context,
            runner: Arc::new(ScriptCodeRunner::new().with_js_limits(JsLimits {
                timeout: Duration::from_secs(60),
                ..JsLimits::default()
            })),
            parent_registry: ctx.registry.clone(),
            parent_ctx: upstream_ctx,
            cancel: cancel.clone(),
        });
        let sub_registry = restricted_computer_subagent_registry(&direct, &machine.name, upstream);
        let model = profile.as_ref().map_or_else(
            || self.default_model.clone(),
            |profile| profile.model.clone(),
        );
        sub_ctx.gate = profile.as_ref().and_then(|profile| {
            crate::tool_gate::build_gate(
                profile.guard.as_ref(),
                sub_registry.clone(),
                self.store.clone(),
                sub_ctx.clone(),
                self.client.clone(),
                model.clone(),
            )
        });
        let agent_config = AgentConfig {
            cancel,
            cost_limit: profile.as_ref().and_then(|profile| profile.cost_limit),
            ..AgentConfig::default()
        };
        let allowed_tools = profile.as_ref().and_then(|profile| {
            profile.allowed_tools.as_ref().map(|tools| {
                let mut tools = tools.clone();
                if !tools.iter().any(|tool| tool == UPSTREAM_TOOL) {
                    tools.push(UPSTREAM_TOOL.to_string());
                }
                tools
            })
        });
        let outcome = run_agent(
            &self.client,
            ChatRequest::new(
                model,
                vec![ChatMessage::system(system), ChatMessage::user(task)],
            ),
            &sub_registry,
            &sub_ctx,
            &agent_config,
            allowed_tools.as_deref(),
        )
        .await
        .map_err(|e| Error::invalid(format!("computer subagent loop failed: {e}")))?;

        Ok(json!({
            "machine": machine.name,
            "profile": profile.as_ref().map(|profile| profile.name.as_str()),
            "content": outcome.content,
            "tool_calls": outcome.tool_invocations.len(),
            "stopped": outcome.stopped,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lazy_store() -> Store {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost/catalerum_test")
            .expect("lazy pool");
        Store::new(pool)
    }

    struct RecordingTool {
        name: &'static str,
        seen: Arc<std::sync::Mutex<Option<Json>>>,
        required: Option<Capability>,
    }

    #[async_trait]
    impl Tool for RecordingTool {
        fn name(&self) -> &str {
            self.name
        }

        fn description(&self) -> &str {
            "test tool"
        }

        fn required_capability(&self) -> Option<Capability> {
            self.required.clone()
        }

        fn parameters_schema(&self) -> Json {
            json!({
                "type": "object",
                "properties": {
                    "agent": { "type": "string" },
                    "value": {}
                },
                "required": ["agent"]
            })
        }

        async fn invoke(&self, args: Json, _ctx: &ToolContext) -> Result<Json> {
            *self.seen.lock().unwrap_or_else(|e| e.into_inner()) = Some(args.clone());
            Ok(args)
        }
    }

    fn recording_tool(name: &'static str) -> (Arc<dyn Tool>, Arc<std::sync::Mutex<Option<Json>>>) {
        recording_tool_with_cap(name, None)
    }

    fn recording_tool_with_cap(
        name: &'static str,
        required: Option<Capability>,
    ) -> (Arc<dyn Tool>, Arc<std::sync::Mutex<Option<Json>>>) {
        let seen = Arc::new(std::sync::Mutex::new(None));
        (
            Arc::new(RecordingTool {
                name,
                seen: seen.clone(),
                required,
            }),
            seen,
        )
    }

    #[tokio::test]
    async fn computer_subagent_tools_accept_named_profiles() {
        let store = lazy_store();
        let client = OpenRouterClient::new("http://localhost:9", "");
        let computer = Arc::new(ComputerRegistry::new("test-pod".into(), None, None));
        let legacy = ComputerAgentTaskTool {
            store: store.clone(),
            client: client.clone(),
            computer: computer.clone(),
            default_model: "default".into(),
        };
        let constrained = ComputerSubagentTool {
            store,
            client,
            computer,
            default_model: "default".into(),
            subagent_runs: SubagentRunManager::default(),
            run_cancel: None,
        };

        for schema in [legacy.parameters_schema(), constrained.parameters_schema()] {
            assert_eq!(schema["properties"]["profile"]["type"], "string");
            assert!(!schema["required"]
                .as_array()
                .expect("required array")
                .iter()
                .any(|name| name == "profile"));
        }
    }

    fn test_upstream(
        source: &str,
        context: Json,
        parent_registry: Option<ToolRegistry>,
    ) -> UpstreamTool {
        UpstreamTool {
            description: "test upstream".to_string(),
            source: source.to_string(),
            context,
            runner: Arc::new(ScriptCodeRunner::new()),
            parent_registry,
            parent_ctx: ToolContext::default(),
            cancel: CancellationToken::new(),
        }
    }

    #[test]
    fn exec_ruling_parse() {
        assert!(matches!(
            parse_exec_ruling("{\"decision\":\"allow\",\"reason\":\"ls is safe\"}"),
            ExecRuling::Allow
        ));
        assert!(matches!(
            parse_exec_ruling("{\"decision\":\"deny\",\"reason\":\"rm -rf /\"}"),
            ExecRuling::Deny(_)
        ));
        assert!(matches!(
            parse_exec_ruling("I think you should ask the user first."),
            ExecRuling::Ask(_)
        ));
        // Garbled → fail closed to ask.
        assert!(matches!(parse_exec_ruling("banana"), ExecRuling::Ask(_)));
    }

    #[test]
    fn computer_search_timeout_defaults_to_ten_seconds_and_is_bounded() {
        assert_eq!(effective_search_timeout_secs(&json!({})), 10);
        assert_eq!(
            effective_search_timeout_secs(&json!({ "timeout_secs": 0 })),
            1
        );
        assert_eq!(
            effective_search_timeout_secs(&json!({ "timeout_secs": 3_601 })),
            MAX_SEARCH_TIMEOUT_SECS
        );
    }

    #[tokio::test]
    async fn restricted_subagent_registry_has_only_pinned_computer_tools_and_upstream() {
        let mut direct = ToolRegistry::new();
        let mut exec_seen = None;
        for &name in SUBAGENT_COMPUTER_TOOLS {
            let (tool, seen) = recording_tool(name);
            if name == "computer_exec" {
                exec_seen = Some(seen);
            }
            direct.register(tool);
        }
        direct.register(recording_tool("computer_list_agents").0);
        direct.register(recording_tool("unrelated_parent_tool").0);
        let upstream = recording_tool(UPSTREAM_TOOL).0;

        let restricted = restricted_computer_subagent_registry(&direct, "workstation", upstream);
        let mut names = restricted.names().map(str::to_string).collect::<Vec<_>>();
        names.sort();
        let mut expected = SUBAGENT_COMPUTER_TOOLS
            .iter()
            .map(|name| (*name).to_string())
            .chain([UPSTREAM_TOOL.to_string()])
            .collect::<Vec<_>>();
        expected.sort();
        assert_eq!(names, expected);
        assert!(!restricted.contains("computer_list_agents"));
        assert!(!restricted.contains("unrelated_parent_tool"));

        let exec = restricted.get("computer_exec").expect("exec wrapper");
        assert!(exec
            .parameters_schema()
            .pointer("/properties/agent")
            .is_none());
        restricted
            .dispatch(
                "computer_exec",
                json!({ "agent": "another-host", "value": 7 }),
                &ToolContext::default(),
            )
            .await
            .expect("pinned dispatch");
        let seen = exec_seen
            .expect("exec recorder")
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .expect("recorded args");
        assert_eq!(seen["agent"], "workstation");
        assert_eq!(seen["value"], 7);
    }

    #[tokio::test]
    async fn upstream_boa_layer_reveals_only_its_return_value() {
        let upstream = test_upstream(
            "return { answer: input.context[input.request.key] };",
            json!({ "public": "shown", "private": "kept behind handler" }),
            None,
        );
        let out = upstream
            .invoke(
                json!({ "request": { "key": "public" } }),
                &ToolContext::default(),
            )
            .await
            .expect("pure upstream handler");
        assert_eq!(out, json!({ "response": { "answer": "shown" } }));
        assert!(!out.to_string().contains("kept behind handler"));
    }

    #[tokio::test]
    async fn upstream_redispatches_parent_tools_but_refuses_recursive_tunnels() {
        let mut parent = ToolRegistry::new();
        parent.register(recording_tool("lookup").0);
        parent.register(recording_tool("computer_subagent").0);
        parent.register(recording_tool_with_cap("secret", cap(Action::Read, "secret")).0);

        let upstream = test_upstream(
            "return catalerum.callTool('lookup', { value: input.request.id });",
            Json::Null,
            Some(parent.clone()),
        );
        let out = upstream
            .invoke(json!({ "request": { "id": 42 } }), &ToolContext::default())
            .await
            .expect("parent redispatch");
        assert_eq!(out["response"]["value"], 42);

        let recursive = test_upstream(
            "return catalerum.callTool('computer_subagent', {});",
            Json::Null,
            Some(parent.clone()),
        );
        let err = recursive
            .invoke(json!({ "request": "recurse" }), &ToolContext::default())
            .await
            .expect_err("recursive subagent must be denied");
        assert!(err.to_string().contains("nested Boa/subagent execution"));

        let mut denied = test_upstream(
            "return catalerum.callTool('secret', {});",
            Json::Null,
            Some(parent),
        );
        denied.parent_ctx.capabilities = Some(Vec::new());
        let err = denied
            .invoke(json!({ "request": "secret" }), &ToolContext::default())
            .await
            .expect_err("parent capability check must still apply");
        assert!(err.to_string().contains("caller's grant does not cover"));
    }

    #[test]
    fn computer_read_file_media_description_is_model_specific() {
        let text = computer_read_file_description("Read.", &["text".into()]);
        assert!(text.contains("unavailable"));
        assert!(text.contains("llmleaf"));

        let image = computer_read_file_description("Read.", &["text".into(), "image".into()]);
        assert!(image.contains("image input"));
        assert!(image.contains("llmleaf"));
        assert!(image.contains("ingested natively"));

        assert_eq!(
            computer_supported_media_path("movie.mp4", &["video".into()]),
            None
        );
        assert_eq!(
            computer_supported_media_path("photo.jpg", &["image".into()]),
            Some("image/jpeg")
        );
    }
}
