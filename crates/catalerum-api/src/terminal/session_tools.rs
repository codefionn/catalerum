//! Session tools (`open_terminal`, `terminal_*`, `persist_terminal`) and the
//! `terminal_subagent` coding worker.

use super::*;

/// Common commands worth advertising when a terminal opens. Availability is
/// probed inside the actual interactive session: backend images differ, so a
/// compile-time or API-host list would be misleading for container/k8s sessions.
pub(super) const TERMINAL_COMMAND_CANDIDATES: [&str; 16] = [
    "python3", "python", "go", "node", "npm", "rustc", "cargo", "git", "sqlite3", "curl", "wget",
    "jq", "make", "gcc", "g++", "java",
];

pub(super) const COMMAND_PROBE_START: &str = "__CATALERUM_COMMANDS__";
pub(super) const COMMAND_PROBE_END: &str = "__CATALERUM_COMMANDS_END__";
pub(super) const MAX_OPEN_STAGE_FILES: usize = 256;

/// Parse the final marked block from a PTY probe. A terminal echoes its input,
/// including the marker literals, so the *last* start marker is the command's
/// output rather than the echoed source line. Only the fixed candidate list is
/// accepted; shell prompts and startup messages can never become advertisements.
pub(super) fn parse_available_commands(output: &str) -> Vec<&'static str> {
    let Some((_, after_start)) = output.rsplit_once(COMMAND_PROBE_START) else {
        return Vec::new();
    };
    let Some((body, _)) = after_start.split_once(COMMAND_PROBE_END) else {
        return Vec::new();
    };
    TERMINAL_COMMAND_CANDIDATES
        .iter()
        .copied()
        .filter(|candidate| body.lines().any(|line| line.trim() == *candidate))
        .collect()
}

/// Check a small, useful command set from inside the newly opened session.
/// Failure is deliberately non-fatal: opening a usable shell is more important
/// than capability advertising, and the response reports that no commands were
/// discovered rather than claiming unverified availability.
async fn probe_available_commands(
    manager: &TerminalManager,
    workspace_id: WorkspaceId,
    session_id: TerminalSessionId,
) -> (Vec<&'static str>, bool) {
    let candidates = TERMINAL_COMMAND_CANDIDATES.join(" ");
    let script = format!(
        "printf '{COMMAND_PROBE_START}\\n'; for cmd in {candidates}; do command -v \"$cmd\" >/dev/null 2>&1 && printf '%s\\n' \"$cmd\"; done; printf '{COMMAND_PROBE_END}\\n'\n"
    );
    if let Err(error) = manager
        .write(workspace_id, session_id, script.into_bytes())
        .await
    {
        tracing::debug!(%error, %session_id, "terminal command probe write failed");
        return (Vec::new(), false);
    }
    match manager
        .read_wait(workspace_id, session_id, 64 * 1024, 2)
        .await
    {
        Ok(output) => {
            let text = String::from_utf8_lossy(&output);
            let complete = text
                .rsplit_once(COMMAND_PROBE_START)
                .is_some_and(|(_, tail)| tail.contains(COMMAND_PROBE_END));
            (parse_available_commands(&text), complete)
        }
        Err(error) => {
            tracing::debug!(%error, %session_id, "terminal command probe read failed");
            (Vec::new(), false)
        }
    }
}

/// `open_terminal` — stand up an interactive terminal session.
pub(super) struct OpenTerminalTool {
    pub(super) manager: Arc<TerminalManager>,
    pub(super) storage: StorageRegistry,
    pub(super) store: Store,
}

#[async_trait]
impl Tool for OpenTerminalTool {
    fn name(&self) -> &str {
        "open_terminal"
    }
    fn required_capability(&self) -> Option<Capability> {
        exec_cap()
    }
    fn description(&self) -> &str {
        "Stand up an interactive, ephemeral terminal you can drive over multiple \
         steps. It runs in a throwaway temp dir — use persist_terminal to keep its \
         files. Returns a `session_id`, a live-checked list of common commands \
         installed in that session, and related tool names. Drive it with \
         terminal_write + terminal_read; use stage_object to bring stored files \
         into it and store_object/persist_terminal to keep outputs. A separate \
         run_command call does not operate in this terminal's private workdir. \
         You may also pass `files` and/or `directories` to stage several stored \
         objects directly while opening the session. After preparing repositories \
         in a no-egress container terminal, `terminal_subagent` can run a \
         profile-style Boa-guarded coding worker over this session."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "backend": {
                    "type": "string",
                    "enum": ["local", "sandbox", "container", "kubernetes"],
                    "description": "Override the runtime for the terminal."
                },
                "files": {
                    "type": "array",
                    "maxItems": MAX_OPEN_STAGE_FILES,
                    "description": "Stored files to stage into the new terminal before returning. Each entry uses the same key/store/dest_path fields as stage_object.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "key": { "type": "string", "description": "Store-relative object key." },
                            "store": { "type": "string", "description": "Source store; omitted uses your default files store." },
                            "dest_path": { "type": "string", "description": "Destination relative to the terminal workdir; omitted uses the source filename." }
                        },
                        "required": ["key"]
                    }
                },
                "directories": {
                    "type": "array",
                    "description": "Stored key prefixes/directories to stage recursively. At most 256 files total may be staged across files and directories.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "prefix": { "type": "string", "description": "Non-empty store-relative directory prefix, e.g. `finance/`." },
                            "store": { "type": "string", "description": "Source store; omitted uses your default files store." },
                            "dest_dir": { "type": "string", "description": "Destination directory relative to the terminal workdir; omitted preserves the source directory's final name." }
                        },
                        "required": ["prefix"]
                    }
                }
            }
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let backend = match args.get("backend").and_then(Json::as_str) {
            Some(s) if !s.trim().is_empty() => Some(
                ExecutorKind::parse_token(s)
                    .ok_or_else(|| Error::invalid(format!("unknown backend `{s}`")))?,
            ),
            _ => None,
        };
        let session = self.manager.open(ws, backend).await?;
        let stager = StageObjectTool {
            manager: self.manager.clone(),
            storage: self.storage.clone(),
            store: self.store.clone(),
        };
        let staging = stager.stage_open_requests(&args, ws, session.id, ctx).await;
        let (available_commands, command_probe_succeeded) =
            probe_available_commands(self.manager.as_ref(), ws, session.id).await;
        Ok(json!({
            "session_id": session.id,
            "backend": session.backend.as_token(),
            "host_dir": session.host_dir,
            "staging": staging,
            "available_commands": available_commands,
            "command_probe": {
                "checked": command_probe_succeeded,
                "candidate_count": TERMINAL_COMMAND_CANDIDATES.len(),
                "note": "Live-checked inside this terminal; the list is useful but not exhaustive."
            },
            "related_tools": [
                "terminal_write", "terminal_read", "stage_object", "read_file",
                "create_file", "edit_file", "store_object", "persist_terminal",
                "close_terminal", "terminal_subagent"
            ],
            "advertise_tools": [
                "terminal_write", "terminal_read", "stage_object", "read_file",
                "create_file", "edit_file", "store_object", "persist_terminal",
                "close_terminal", "terminal_subagent"
            ],
            "usage_note": "Use terminal_write and terminal_read with this session_id for commands that must see this terminal's files. Do not use run_command for staged/session files."
        }))
    }
}

/// `terminal_write` — send input (a command line) to a session.
pub(super) struct TerminalWriteTool {
    pub(super) manager: Arc<TerminalManager>,
}

#[async_trait]
impl Tool for TerminalWriteTool {
    fn name(&self) -> &str {
        "terminal_write"
    }
    fn required_capability(&self) -> Option<Capability> {
        exec_cap()
    }
    fn description(&self) -> &str {
        "Write input to a terminal session's stdin. Include a trailing newline \
         in `data` to run a command. Read the result with terminal_read. Use this \
         (not run_command) for commands that must access files created by \
         create_file or copied into this session by stage_object."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "description": "From open_terminal." },
                "data": { "type": "string", "description": "Bytes to write (add \\n to execute)." }
            },
            "required": ["session_id", "data"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let id = session_id_arg(&args)?;
        let data = args
            .get("data")
            .and_then(Json::as_str)
            .ok_or_else(|| Error::invalid("`data` is required"))?;
        self.manager.write(ws, id, data.as_bytes().to_vec()).await?;
        Ok(json!({ "ok": true }))
    }
}

/// `terminal_read` — drain a session's accumulated output.
pub(super) struct TerminalReadTool {
    pub(super) manager: Arc<TerminalManager>,
}

#[async_trait]
impl Tool for TerminalReadTool {
    fn name(&self) -> &str {
        "terminal_read"
    }
    fn required_capability(&self) -> Option<Capability> {
        exec_cap()
    }
    fn description(&self) -> &str {
        "Read output a terminal session has produced since your last read. \
         Returns `output` (decoded text). Poll after terminal_write, or set \
         `wait_secs` to block until a command's output settles before returning."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "description": "From open_terminal." },
                "wait_secs": { "type": "integer", "description": "Block up to N seconds, draining until output goes quiet, before returning (optional). Use after running a command so a single read captures its full output." },
                "max_bytes": { "type": "integer", "description": "Cap on bytes returned (optional)." }
            },
            "required": ["session_id"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let id = session_id_arg(&args)?;
        let max = usize::try_from(args.get("max_bytes").and_then(Json::as_u64).unwrap_or(0))
            .unwrap_or(usize::MAX);
        // Clamp the wait so an automation node can't block the worker indefinitely.
        let wait_secs = args
            .get("wait_secs")
            .and_then(Json::as_u64)
            .unwrap_or(0)
            .min(600);
        let bytes = self.manager.read_wait(ws, id, max, wait_secs).await?;
        Ok(json!({ "output": String::from_utf8_lossy(&bytes) }))
    }
}

/// `list_terminals` — list a workspace's active sessions.
pub(super) struct ListTerminalsTool {
    pub(super) manager: Arc<TerminalManager>,
}

#[async_trait]
impl Tool for ListTerminalsTool {
    fn name(&self) -> &str {
        "list_terminals"
    }
    fn required_capability(&self) -> Option<Capability> {
        exec_cap()
    }
    fn description(&self) -> &str {
        "List the active terminal sessions in this workspace."
    }
    fn parameters_schema(&self) -> Json {
        json!({ "type": "object", "properties": {} })
    }
    async fn invoke(&self, _args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let sessions = self.manager.list(ws).await?;
        Ok(json!(sessions
            .iter()
            .map(|s| json!({
                "session_id": s.id,
                "backend": s.backend.as_token(),
                "host_dir": s.host_dir,
            }))
            .collect::<Vec<_>>()))
    }
}

/// `close_terminal` — end a session.
pub(super) struct CloseTerminalTool {
    pub(super) manager: Arc<TerminalManager>,
}

#[async_trait]
impl Tool for CloseTerminalTool {
    fn name(&self) -> &str {
        "close_terminal"
    }
    fn required_capability(&self) -> Option<Capability> {
        exec_cap()
    }
    fn description(&self) -> &str {
        "Close a terminal session and free its resources. An ephemeral \
         session's temp dir is discarded (persist_terminal first to keep files)."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": { "session_id": { "type": "string", "description": "From open_terminal." } },
            "required": ["session_id"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let id = session_id_arg(&args)?;
        self.manager.close(ws, id).await?;
        Ok(json!({ "ok": true }))
    }
}

/// `persist_terminal` — flush a session's working dir to object storage.
pub(super) struct PersistTerminalTool {
    pub(super) manager: Arc<TerminalManager>,
}

#[async_trait]
impl Tool for PersistTerminalTool {
    fn name(&self) -> &str {
        "persist_terminal"
    }
    fn required_capability(&self) -> Option<Capability> {
        // `exec:run`, like the rest of the terminal surface (a protected scope no
        // base role holds, §19). Dispatch enforces a single capability, so we gate
        // on the security-critical scope rather than the weaker `storage:write` a
        // base Member already has. Only registered when an object store exists.
        exec_cap()
    }
    fn description(&self) -> &str {
        "Snapshot a terminal session's working directory to object storage under \
         `prefix` (e.g. \"runs/2026-06-25\"), optionally only the `source_subdir` \
         folder of it (e.g. a cloned repo's \"docs\"). Each written file is catalogued \
         and fires a storage-change event, so this can feed a downstream indexing \
         automation. Use for an ephemeral terminal whose files you want to keep. \
         Returns the object keys written."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "description": "From open_terminal." },
                "prefix": { "type": "string", "description": "Object-key prefix to write files under." },
                "source_subdir": { "type": "string", "description": "Optional: copy only this subdir of the working directory (relative, no '..'). Omit to copy the whole workdir." }
            },
            "required": ["session_id", "prefix"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let id = session_id_arg(&args)?;
        let prefix = args
            .get("prefix")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::invalid("`prefix` is required"))?;
        let source_subdir = args.get("source_subdir").and_then(Json::as_str);
        let keys = self.manager.persist(ws, id, prefix, source_subdir).await?;
        Ok(json!({ "keys": keys }))
    }
}

// ---------------------------------------------------------------------------
// terminal_subagent — isolated coding worker + profile-style Boa policy
// ---------------------------------------------------------------------------

const TERMINAL_SUBAGENT_NAME: &str = "terminal_subagent";
pub(super) const TERMINAL_UPSTREAM_NAME: &str = "upstream";
pub(super) const TERMINAL_FINISH_NAME: &str = "finish_work";
const MAX_SUBAGENT_SCRIPT_BYTES: usize = 64 * 1024;
pub(super) const TERMINAL_SUBAGENT_TOOLS: &[&str] = &[
    "terminal_write",
    "terminal_read",
    "read_file",
    "create_file",
    "edit_file",
];

/// Pin an existing terminal tool to the session prepared by the parent. The
/// child never receives `open_terminal`, staging, persistence, or session-list
/// tools and cannot redirect an allowed operation to another session id.
struct PinnedTerminalTool {
    inner: Arc<dyn Tool>,
    session_id: TerminalSessionId,
    description: String,
}

#[async_trait]
impl Tool for PinnedTerminalTool {
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
            properties.remove("session_id");
        }
        if let Some(required) = schema.get_mut("required").and_then(Json::as_array_mut) {
            required.retain(|value| value.as_str() != Some("session_id"));
        }
        schema
    }

    async fn invoke(&self, mut args: Json, ctx: &ToolContext) -> Result<Json> {
        let object = args
            .as_object_mut()
            .ok_or_else(|| Error::invalid("terminal tool arguments must be an object"))?;
        object.insert("session_id".into(), json!(self.session_id));
        self.inner.invoke(args, ctx).await
    }
}

/// The child's sole route back to the parent's registry. The proxy has a static
/// parent-selected tool-name allow-list; the profile-compatible Boa gate layered
/// on the child then inspects `input.args.tool` and
/// `input.args.arguments` to constrain exact PR/story identifiers and operations.
pub(super) struct TerminalUpstreamTool {
    pub(super) description: String,
    pub(super) allowed_tools: Vec<String>,
    pub(super) parent_registry: ToolRegistry,
    pub(super) parent_ctx: ToolContext,
    pub(super) cancel: CancellationToken,
}

impl TerminalUpstreamTool {
    fn recursive(tool: &str) -> bool {
        matches!(
            tool,
            "run_javascript"
                | "delegate"
                | "open_terminal"
                | "terminal_subagent"
                | "computer_subagent"
                | "computer_agent_task"
                | "monitor_subagent"
                | "wait_subagent"
                | "stop_subagent"
        )
    }

    fn blocked(tool: &str) -> bool {
        Self::recursive(tool)
            || matches!(
                tool,
                "run_command"
                    | "terminal_write"
                    | "terminal_read"
                    | "read_file"
                    | "create_file"
                    | "edit_file"
                    | "list_terminals"
                    | "close_terminal"
                    | "persist_terminal"
                    | "stage_object"
                    | "store_object"
            )
            || tool.starts_with("computer_")
    }
}

#[async_trait]
impl Tool for TerminalUpstreamTool {
    fn name(&self) -> &str {
        TERMINAL_UPSTREAM_NAME
    }
    /// Open: child-run plumbing; nested calls dispatch under the parent context.
    fn required_capability(&self) -> Option<Capability> {
        None
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "tool": {
                    "type": "string",
                    "enum": self.allowed_tools,
                    "description": "The parent tool to request through the policy boundary."
                },
                "arguments": {
                    "type": "object",
                    "description": "Arguments for that parent tool. The Boa policy inspects these before dispatch."
                }
            },
            "required": ["tool", "arguments"]
        })
    }

    async fn invoke(&self, args: Json, _ctx: &ToolContext) -> Result<Json> {
        if self.cancel.is_cancelled() {
            return Err(Error::other("terminal subagent was stopped"));
        }
        let tool = args
            .get("tool")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Error::invalid("upstream requires `tool`"))?;
        if Self::blocked(tool) || !self.allowed_tools.iter().any(|allowed| allowed == tool) {
            return Err(Error::unauthorized(format!(
                "`{tool}` is not on this terminal subagent's upstream allow-list"
            )));
        }
        let arguments = args
            .get("arguments")
            .cloned()
            .filter(Json::is_object)
            .ok_or_else(|| Error::invalid("upstream `arguments` must be an object"))?;
        self.parent_registry
            .dispatch(tool, arguments, &self.parent_ctx)
            .await
    }
}

#[derive(Clone, Debug)]
pub(super) struct GoalVerdict {
    met: bool,
    analysis: String,
    pub(super) evidence: Vec<String>,
    summary: String,
}

impl GoalVerdict {
    fn as_json(&self) -> Json {
        json!({
            "met": self.met,
            "analysis": self.analysis,
            "evidence": self.evidence,
            "summary": self.summary,
        })
    }
}

pub(super) fn finalizer_reported_push(goal_met: bool, result: &Json, error: Option<&str>) -> bool {
    goal_met
        && error.is_none()
        && result
            .get("pushed")
            .and_then(Json::as_bool)
            .unwrap_or(false)
}

/// A structured terminal condition for the child run. The finalizer never
/// trusts free-form assistant prose: it runs only when this tool recorded
/// `goal_met=true`, with the analysis + evidence handed to the parent script.
pub(super) struct FinishTerminalWorkTool {
    pub(super) verdict: Arc<Mutex<Option<GoalVerdict>>>,
}

#[async_trait]
impl Tool for FinishTerminalWorkTool {
    fn name(&self) -> &str {
        TERMINAL_FINISH_NAME
    }
    /// Open: records the child run's result, touches no tenant data.
    fn required_capability(&self) -> Option<Capability> {
        None
    }

    fn description(&self) -> &str {
        "Finish the assigned work with a structured goal assessment. Call exactly once after \
         inspecting the diff and running the available verification. Set goal_met=true only \
         when the evidence demonstrates the requested goal is satisfied. Only a true verdict \
         permits the parent-side finalizer to publish upstream."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "goal_met": { "type": "boolean" },
                "analysis": { "type": "string", "description": "Why the goal is or is not met, tied to acceptance criteria." },
                "evidence": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Concrete evidence such as tests, diff inspection, and remaining failures."
                },
                "summary": { "type": "string", "description": "Concise implementation summary for the parent/PR." }
            },
            "required": ["goal_met", "analysis", "evidence", "summary"]
        })
    }

    async fn invoke(&self, args: Json, _ctx: &ToolContext) -> Result<Json> {
        let met = args
            .get("goal_met")
            .and_then(Json::as_bool)
            .ok_or_else(|| Error::invalid("finish_work requires boolean `goal_met`"))?;
        let required_text = |name: &str| -> Result<String> {
            args.get(name)
                .and_then(Json::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .ok_or_else(|| Error::invalid(format!("finish_work requires non-empty `{name}`")))
        };
        let evidence = args
            .get("evidence")
            .and_then(Json::as_array)
            .ok_or_else(|| Error::invalid("finish_work requires `evidence` array"))?
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
                    .ok_or_else(|| {
                        Error::invalid("finish_work evidence entries must be non-empty strings")
                    })
            })
            .collect::<Result<Vec<_>>>()?;
        if evidence.is_empty() {
            return Err(Error::invalid(
                "finish_work requires at least one evidence entry",
            ));
        }
        let verdict = GoalVerdict {
            met,
            analysis: required_text("analysis")?,
            evidence,
            summary: required_text("summary")?,
        };
        let mut slot = self
            .verdict
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if slot.is_some() {
            return Err(Error::invalid("finish_work may be called only once"));
        }
        *slot = Some(verdict.clone());
        Ok(json!({ "recorded": true, "goal": verdict.as_json() }))
    }
}

/// Boa host for the post-goal parent finalizer. Calls run through the parent's
/// exact registry/context, so static capabilities, the parent's own tool guard,
/// dry-run simulation, and approvals still apply. Recursive agent/Boa entry
/// points remain blocked.
struct TerminalFinalizerHost {
    registry: ToolRegistry,
    parent_ctx: ToolContext,
    cancel: CancellationToken,
}

impl catalerum_script::UiScriptHost for TerminalFinalizerHost {
    fn call_tool(&self, tool: &str, args: Json) -> std::result::Result<Json, String> {
        if self.cancel.is_cancelled() {
            return Err("terminal subagent was stopped; finalization is disabled".into());
        }
        if TerminalUpstreamTool::recursive(tool) {
            return Err(format!(
                "terminal finalizer cannot call `{tool}` — recursive agent/Boa execution is disabled"
            ));
        }
        tokio::runtime::Handle::current()
            .block_on(self.registry.dispatch(tool, args, &self.parent_ctx))
            .map_err(|error| error.to_string())
    }
}

fn parse_string_array(args: &Json, name: &str) -> Result<Vec<String>> {
    let values = args
        .get(name)
        .and_then(Json::as_array)
        .ok_or_else(|| Error::invalid(format!("terminal_subagent requires `{name}` array")))?;
    let mut out = Vec::with_capacity(values.len());
    for value in values {
        let value = value
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                Error::invalid(format!(
                    "terminal_subagent `{name}` entries must be non-empty strings"
                ))
            })?;
        if !out.iter().any(|existing| existing == value) {
            out.push(value.to_string());
        }
    }
    if out.is_empty() {
        return Err(Error::invalid(format!(
            "terminal_subagent requires at least one `{name}` entry"
        )));
    }
    Ok(out)
}

pub(super) fn restricted_terminal_subagent_registry(
    parent: &ToolRegistry,
    session_id: TerminalSessionId,
    upstream: Arc<dyn Tool>,
    finish: Arc<dyn Tool>,
) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    for name in TERMINAL_SUBAGENT_TOOLS {
        if let Some(inner) = parent.get(name) {
            let description = format!(
                "{} This subagent-scoped instance is pinned to terminal `{session_id}`; no \
                 `session_id` argument is accepted.",
                inner.description()
            );
            registry.register(Arc::new(PinnedTerminalTool {
                inner,
                session_id,
                description,
            }));
        }
    }
    registry.register(upstream);
    registry.register(finish);
    registry
}

#[derive(Clone)]
pub(super) struct TerminalSubagentTool {
    pub(super) manager: Arc<TerminalManager>,
    pub(super) store: Store,
    pub(super) client: OpenRouterClient,
    pub(super) default_model: String,
    pub(super) subagent_runs: SubagentRunManager,
    /// Set only on the private tool clone owned by a background run.
    pub(super) run_cancel: Option<CancellationToken>,
}

#[async_trait]
impl Tool for TerminalSubagentTool {
    fn name(&self) -> &str {
        TERMINAL_SUBAGENT_NAME
    }

    fn required_capability(&self) -> Option<Capability> {
        exec_cap()
    }

    fn description(&self) -> &str {
        "Run a coding subagent in an existing no-egress terminal after the parent has acquired a \
         central work item, created its PR, and staged the repository/repositories. The child sees \
         only terminal operations pinned to that session, `upstream`, and `finish_work`. `upstream` \
         can call only the parent tool names in `upstream_tools`; a profile-compatible Boa \
         `guard_script` reviews EVERY child call and output with `input.phase`, `input.tool`, \
         `input.args`, `input.output`, and immutable `input.policy_context`, so it can enforce the \
         exact PR and user-story ids and read/write operations. The child cannot publish directly \
         because its terminal must be OS network-isolated. It records a structured goal verdict; \
         only when goal_met=true does the parent-authored `finalize_code` run, under the parent's \
         original authority, to update/push upstream. The finalizer must return `{pushed: true}` \
         only after publication actually succeeds. Pass `profile` to use a named agent profile's \
         model, instructions, skills, tool restrictions, guard, and attenuated grant. Set \
         `background=true` to return a run id \
         immediately; the parent can monitor, wait for, or stop that run."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "description": "Existing no-egress terminal prepared with the repositories." },
                "goal": { "type": "string", "description": "Implementation goal and acceptance criteria from the user story." },
                "profile": {
                    "type": "string",
                    "description": "Optional named agent profile to configure and scope the subagent instead of using the workspace default model."
                },
                "upstream_tools": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Exact parent tool names the child may request through `upstream` (normally the PR and user-story provider tools only)."
                },
                "upstream_description": { "type": "string", "description": "Child-facing contract explaining the allowed PR/story operations and argument shapes." },
                "policy_context": {
                    "type": "object",
                    "description": "Immutable scope bound into every Boa review as `input.policy_context`, e.g. `{pull_request:{id,...}, user_story:{id,...}}`. The child cannot alter it."
                },
                "guard_script": {
                    "type": "string",
                    "description": "Profile-style Boa function body returning allow/deny/ask (or `{decision,reason}`). It reviews call and output phases and can inspect `input.args`, including `input.args.tool` + `input.args.arguments` for upstream requests. Failures deny."
                },
                "finalize_code": {
                    "type": "string",
                    "description": "Boa function body run by the parent only after `finish_work.goal_met=true`. It receives `input.goal`, `input.policy_context`, `input.session_id`, and `input.agent_content`; may call parent tools via `catalerum.callTool`; return an object with boolean `pushed` plus details."
                },
                "background": {
                    "type": "boolean",
                    "default": false,
                    "description": "Start on this API pod and return a controllable run id immediately."
                }
            },
            "required": ["session_id", "goal", "upstream_tools", "upstream_description", "policy_context", "guard_script", "finalize_code"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        if ctx.ui_id.is_some() {
            return Err(Error::unauthorized(
                "terminal_subagent is unavailable from an emerged-UI handler",
            ));
        }
        if self.run_cancel.is_none()
            && args
                .get("background")
                .and_then(Json::as_bool)
                .unwrap_or(false)
        {
            let goal = args
                .get("goal")
                .and_then(Json::as_str)
                .ok_or_else(|| Error::invalid("terminal_subagent requires `goal`"))?;
            let label = goal.chars().take(160).collect::<String>();
            let mut run_args = args;
            if let Some(object) = run_args.as_object_mut() {
                object.remove("background");
            }
            let template = self.clone();
            let parent_ctx = ctx.clone();
            return self
                .subagent_runs
                .spawn(
                    ctx,
                    TERMINAL_SUBAGENT_NAME,
                    label,
                    move |cancel| async move {
                        let runner: Arc<dyn Tool> = Arc::new(Self {
                            run_cancel: Some(cancel),
                            ..template
                        });
                        runner.invoke(run_args, &parent_ctx).await
                    },
                )
                .await;
        }
        let ws = workspace(ctx)?;
        let profile_name = args
            .get("profile")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let profile = if let Some(name) = profile_name {
            let parent_caps = ctx.capabilities.as_deref().ok_or_else(|| {
                Error::unauthorized("a named subagent profile requires a capability-scoped caller")
            })?;
            Some(
                resolve_constrained_profile(
                    &self.store,
                    ws,
                    &self.default_model,
                    name,
                    parent_caps,
                )
                .await?,
            )
        } else {
            None
        };
        let session_id = session_id_arg(&args)?;
        self.manager
            .require_subagent_isolation(ws, session_id)
            .await?;
        let goal = args
            .get("goal")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Error::invalid("terminal_subagent requires non-empty `goal`"))?
            .to_string();
        let upstream_tools = parse_string_array(&args, "upstream_tools")?;
        if upstream_tools
            .iter()
            .any(|tool| TerminalUpstreamTool::blocked(tool))
        {
            return Err(Error::invalid(
                "terminal_subagent `upstream_tools` contains a recursive/unsafe framework tool",
            ));
        }
        let parent_registry = ctx.registry.clone().ok_or_else(|| {
            Error::unauthorized(
                "terminal_subagent requires a dispatching parent registry (direct invoke refused)",
            )
        })?;
        for tool in &upstream_tools {
            if !parent_registry.contains(tool) {
                return Err(Error::invalid(format!(
                    "terminal_subagent upstream tool `{tool}` is not in the parent's registry"
                )));
            }
        }
        let upstream_description = args
            .get("upstream_description")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                Error::invalid("terminal_subagent requires non-empty `upstream_description`")
            })?
            .to_string();
        let policy_context = args
            .get("policy_context")
            .cloned()
            .filter(Json::is_object)
            .ok_or_else(|| {
                Error::invalid("terminal_subagent `policy_context` must be an object")
            })?;
        let guard_script = args
            .get("guard_script")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Error::invalid("terminal_subagent requires non-empty `guard_script`"))?
            .to_string();
        let finalize_code = args
            .get("finalize_code")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Error::invalid("terminal_subagent requires non-empty `finalize_code`"))?
            .to_string();
        for (name, source) in [
            ("guard_script", guard_script.as_str()),
            ("finalize_code", finalize_code.as_str()),
        ] {
            if source.len() > MAX_SUBAGENT_SCRIPT_BYTES {
                return Err(Error::invalid(format!(
                    "terminal_subagent `{name}` exceeds the {MAX_SUBAGENT_SCRIPT_BYTES}-byte limit"
                )));
            }
        }
        let cancel = self.run_cancel.clone().unwrap_or_default();

        let mut bound_policy_context = policy_context.clone();
        if let Some(object) = bound_policy_context.as_object_mut() {
            object.insert("session_id".into(), json!(session_id));
            object.insert("upstream_tools".into(), json!(upstream_tools));
        }
        let verdict = Arc::new(Mutex::new(None));
        let mut upstream_ctx = ctx.clone();
        if let Some(profile) = &profile {
            upstream_ctx.grant_id = profile.grant_id;
            upstream_ctx.capabilities = Some(profile.capabilities.clone());
            upstream_ctx.dry_run |= profile.dry_run;
        }
        let upstream: Arc<dyn Tool> = Arc::new(TerminalUpstreamTool {
            description: format!(
                "Parent-mediated PR/user-story channel. {upstream_description} Every request and \
                 response is inspected by the Boa policy before it is usable."
            ),
            allowed_tools: upstream_tools,
            parent_registry: parent_registry.clone(),
            parent_ctx: upstream_ctx,
            cancel: cancel.clone(),
        });
        let finish: Arc<dyn Tool> = Arc::new(FinishTerminalWorkTool {
            verdict: verdict.clone(),
        });
        let child_registry =
            restricted_terminal_subagent_registry(&parent_registry, session_id, upstream, finish);

        let guard = ToolGuard {
            script: Some(guard_script),
            llm: None,
            object_labels: None,
            on_error: GuardFail::Deny,
        };
        let mut child_ctx = ToolContext {
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
        let model = profile.as_ref().map_or_else(
            || self.default_model.clone(),
            |profile| profile.model.clone(),
        );
        let boundary_gate = crate::tool_gate::build_gate_with_context(
            Some(&guard),
            // Unlike a general profile, this constrained guard needs only its
            // bound policy context + optional classifyWithLlm. Giving its script
            // an empty lookup registry prevents the policy itself from bypassing
            // `upstream` or recursively spawning work through callTool.
            ToolRegistry::new(),
            self.store.clone(),
            ctx.clone(),
            self.client.clone(),
            model.clone(),
            Some(bound_policy_context.clone()),
        );
        let profile_gate = profile.as_ref().and_then(|profile| {
            crate::tool_gate::build_gate(
                profile.guard.as_ref(),
                child_registry.clone(),
                self.store.clone(),
                child_ctx.clone(),
                self.client.clone(),
                model.clone(),
            )
        });
        child_ctx.gate = crate::tool_gate::all_gates([boundary_gate, profile_gate]);

        let assignment = format!(
            "You are a focused coding subagent working in one prepared, no-egress terminal. All \
             terminal tools are pinned to that session. Implement this goal and its acceptance \
             criteria:\n\n{goal}\n\nThe parent already acquired the work, created the PR, and staged every \
             repository. Do not clone, fetch, publish, or access anything outside the prepared \
             terminal. For PR/user-story information and updates, use only `upstream`: \
             {upstream_description} Every call and output is reviewed by a Boa policy that sees \
             the full tool parameters; a denial is a boundary, not something to bypass. Inspect \
             the final diff and run relevant verification. Then call `finish_work` exactly once \
             with goal analysis and concrete evidence. Set goal_met=true only when the evidence \
             satisfies the goal. Do not claim a push: publication happens parent-side only after \
             that verdict. Return a concise final summary after `finish_work`."
        );
        let system = profile.as_ref().map_or_else(
            || assignment.clone(),
            |profile| {
                format!(
                    "{}\n\n# Constrained terminal assignment\n\n{assignment}",
                    profile.system
                )
            },
        );
        let agent_config = AgentConfig {
            cancel: cancel.clone(),
            cost_limit: profile.as_ref().and_then(|profile| profile.cost_limit),
            ..AgentConfig::default()
        };
        let allowed_tools = profile.as_ref().and_then(|profile| {
            profile.allowed_tools.as_ref().map(|tools| {
                let mut tools = tools.clone();
                for required in [TERMINAL_UPSTREAM_NAME, TERMINAL_FINISH_NAME] {
                    if !tools.iter().any(|tool| tool == required) {
                        tools.push(required.to_string());
                    }
                }
                tools
            })
        });
        let outcome = run_agent(
            &self.client,
            ChatRequest::new(
                model,
                vec![ChatMessage::system(system), ChatMessage::user(goal.clone())],
            ),
            &child_registry,
            &child_ctx,
            &agent_config,
            allowed_tools.as_deref(),
        )
        .await
        .map_err(|error| Error::provider(format!("terminal subagent loop failed: {error}")))?;

        // Never hold the sync mutex across the awaited Boa finalizer. The agent
        // loop is complete, so a short clone is sufficient and deterministic.
        let recorded = verdict
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let stopped = outcome.stopped || cancel.is_cancelled();
        let goal_verdict = if stopped {
            GoalVerdict {
                met: false,
                analysis: "The subagent was stopped; its goal is not accepted and publication is disabled."
                    .into(),
                evidence: vec!["The parent requested cancellation before completion.".into()],
                summary: outcome.content.clone(),
            }
        } else {
            recorded.unwrap_or_else(|| GoalVerdict {
                met: false,
                analysis:
                    "The subagent ended without calling finish_work; the goal is not accepted."
                        .into(),
                evidence: vec!["No structured finish_work verdict was recorded.".into()],
                summary: outcome.content.clone(),
            })
        };

        let mut finalize_attempted = false;
        let mut finalize_result = Json::Null;
        let mut finalize_error = None;
        if goal_verdict.met && !cancel.is_cancelled() {
            finalize_attempted = true;
            let input = json!({
                "goal": goal_verdict.as_json(),
                "policy_context": bound_policy_context,
                "session_id": session_id,
                "agent_content": outcome.content,
            });
            let host = Arc::new(TerminalFinalizerHost {
                registry: parent_registry,
                parent_ctx: ctx.clone(),
                cancel: cancel.clone(),
            });
            let runner = ScriptCodeRunner::new().with_js_limits(JsLimits {
                timeout: std::time::Duration::from_secs(60),
                ..JsLimits::default()
            });
            match runner.eval_with_host(&finalize_code, &input, host).await {
                Ok(result) => finalize_result = result,
                Err(error) => finalize_error = Some(error),
            }
        }
        let pushed_upstream = finalizer_reported_push(
            goal_verdict.met,
            &finalize_result,
            finalize_error.as_deref(),
        );

        Ok(json!({
            "session_id": session_id,
            "profile": profile.as_ref().map(|profile| profile.name.as_str()),
            "content": outcome.content,
            "tool_calls": outcome.tool_invocations.len(),
            "stopped": stopped || cancel.is_cancelled(),
            "goal": goal_verdict.as_json(),
            "finalization": {
                "attempted": finalize_attempted,
                "result": finalize_result,
                "error": finalize_error,
                "pushed_upstream": pushed_upstream,
            }
        }))
    }
}
