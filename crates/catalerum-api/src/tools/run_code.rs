//! `run_command` / `run_javascript` execution tools.

use super::*;

/// `run_command` — run a shell command via the configured [`Executor`] (SOUL
/// §20). **Protected:** requires `exec:run`, which no base role grants — so it is
/// denied by default and reachable only by an agent explicitly handed that
/// capability. The executor (e.g. [`LocalExecutor`](catalerum_exec::LocalExecutor))
/// adds its own allow-list on top.
pub(crate) struct RunCommandTool {
    /// Per-call executor backend (used when the per-workspace sandbox is off).
    pub(crate) executor: Option<Arc<dyn Executor>>,
    /// Per-workspace sandbox manager — when set, the command runs inside the
    /// calling workspace's single long-lived sandbox (SOUL §20).
    pub(crate) sandbox: Option<Arc<WorkspaceSandboxManager>>,
}

#[async_trait]
impl Tool for RunCommandTool {
    fn name(&self) -> &str {
        "run_command"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Run, "exec")
    }
    fn description(&self) -> &str {
        "Run one non-interactive shell command in the workspace sandbox or \
         executor's default working directory. `command` is an argv array (e.g. \
         [\"ls\", \"-la\"]); this tool does not accept a terminal `session_id` and \
         does not run in an open_terminal session's private workdir. To operate on \
         files copied by stage_object or created in a terminal, use terminal_write \
         with that same session_id and collect output with terminal_read. Returns \
         exit_code, stdout, stderr. Subject to an allow-list; long runs time out."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Program + arguments, e.g. [\"echo\", \"hi\"]."
                },
                "stdin": { "type": "string", "description": "Data to pipe to stdin (optional)." },
                "timeout_secs": { "type": "integer", "description": "Wall-clock timeout (optional)." }
            },
            "required": ["command"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let argv: Vec<String> = args
            .get("command")
            .and_then(Json::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Json::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if argv.is_empty() {
            return Err(Error::invalid(
                "`command` is required and must be a non-empty argv array",
            ));
        }
        let spec = CommandSpec {
            argv,
            stdin: args.get("stdin").and_then(Json::as_str).map(str::to_string),
            timeout_secs: args.get("timeout_secs").and_then(Json::as_u64),
            ..Default::default()
        };
        // Prefer the per-workspace sandbox (the command runs *inside* the
        // workspace's single long-lived sandbox); fall back to the per-call
        // executor backend when the sandbox is off.
        let result = if let Some(sandbox) = &self.sandbox {
            let ws = workspace(ctx)?;
            sandbox.run(ws, spec).await?
        } else if let Some(executor) = &self.executor {
            executor.run(spec).await?
        } else {
            return Err(Error::invalid("no executor backend is configured"));
        };
        Ok(serde_json::to_value(result)?)
    }
}

/// `run_javascript` — evaluate a JavaScript snippet in the **Boa sandbox**
/// (SOUL §11). Boa ships **no** host functions — no filesystem, network, or clock
/// — and this tool installs exactly one bridge: `catalerum.callTool(name, args)`,
/// which **re-dispatches a registry tool under the calling context** (the registry
/// arrives via [`ToolContext::registry`], injected by dispatch itself). A nested
/// call therefore passes the *identical* deny-by-default capability check, tool
/// guard, and dry-run simulation a directly-issued call would — the script wields
/// only the authority its caller already holds, never more. The tool itself stays
/// **ungated** like the pure `html_to_markdown` / `extract_html` transforms: with
/// no `callTool` use it is the same side-effect-free computation as before, and
/// every side effect it *can* reach is gated per nested call.
///
/// Two nested surfaces are refused by the host, fail-closed:
/// - `run_javascript` itself — self-recursion would stack Boa evals across
///   blocking threads; a script defines a JS function instead.
/// - any call when the context carries a [`ui_id`](ToolContext::ui_id) — an
///   emerged-UI handler is confined to the `[ui].handler_tools` allow-list at its
///   own `callTool` entry (SOUL §12), and tunnelling through this tool's bridge
///   would bypass that boundary.
///
/// Execution is bounded by loop-iteration, recursion, and VM-stack limits plus a
/// wall-clock timeout (raised at registration to leave room for nested I/O), so a
/// runaway script fails deterministically instead of hanging. Contrast
/// `run_command`, which reaches a real shell and is deny-by-default behind
/// `exec:run`.
pub(crate) const RUN_JAVASCRIPT_NAME: &str = "run_javascript";

pub(crate) struct RunJavascriptTool {
    /// The runner (no executor, no automation tool host — the nested-call host is
    /// built per invocation from the calling context).
    pub(crate) runner: Arc<ScriptCodeRunner>,
}

/// The per-invocation [`UiScriptHost`] backing `catalerum.callTool` inside a
/// `run_javascript` eval: re-dispatches the named tool through the registry the
/// outer call was dispatched from, under the outer call's own [`ToolContext`] —
/// so authorization (capability check, gate, dry-run) is the registry's, applied
/// per nested call exactly as for a top-level call.
///
/// `call_tool` is synchronous and runs on the script's `spawn_blocking` thread
/// (Boa's `Context` is `!Send`), never a runtime worker — so `block_on` is valid
/// (same pattern as the automation code-node host).
struct JsToolHost {
    registry: ToolRegistry,
    ctx: ToolContext,
}

impl catalerum_script::UiScriptHost for JsToolHost {
    fn call_tool(&self, tool: &str, args: Json) -> std::result::Result<Json, String> {
        if tool == RUN_JAVASCRIPT_NAME {
            return Err(
                "run_javascript cannot call itself — define and call a plain JS function instead"
                    .to_string(),
            );
        }
        let handle = tokio::runtime::Handle::current();
        handle
            .block_on(self.registry.dispatch(tool, args, &self.ctx))
            .map_err(|e| e.to_string())
    }
}

#[async_trait]
impl Tool for RunJavascriptTool {
    fn name(&self) -> &str {
        RUN_JAVASCRIPT_NAME
    }
    fn description(&self) -> &str {
        "Evaluate JavaScript in a secure sandbox and return its result. `code` is a \
         function BODY: `return` a value to produce the result (e.g. \
         `return input.a + input.b`). The optional `input` is any JSON value, bound \
         as the global `input`. The sandbox has no filesystem, network, or clock, \
         but `catalerum.callTool(name, args)` synchronously dispatches any other \
         registered tool and returns its JSON result — each nested call is \
         authorization-checked exactly like a normal tool call, and a denied or \
         failed one throws a catchable Error. It cannot call run_javascript itself. \
         Use it to chain, loop over, or aggregate tool calls with exact logic \
         (e.g. iterate a list result and act per item), or for pure arithmetic and \
         JSON/string transforms. Runaway loops/recursion are bounded and the whole \
         call times out."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "JavaScript function body; `return` a value, e.g. `return input.n * 2`. May call other tools via `catalerum.callTool(name, args)`. A body with no `return` yields null."
                },
                "input": {
                    "description": "Optional JSON value bound as the global `input` inside the script."
                }
            },
            "required": ["code"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let code = required_str(&args, "code")?;
        let input = args.get("input").cloned().unwrap_or(Json::Null);
        // The nested-call bridge needs the dispatching registry (injected by
        // `ToolRegistry::dispatch`); without one (a direct `invoke`, unit tests)
        // the eval stays the pure transform it always was. A UI-handler context
        // (`ui_id`) also stays pure: its allow-list boundary lives at the
        // handler's own `callTool` entry and must not be tunnelled through here.
        let host = match (&ctx.registry, ctx.ui_id) {
            (Some(registry), None) => Some(Arc::new(JsToolHost {
                registry: registry.clone(),
                ctx: ctx.clone(),
            })
                as Arc<dyn catalerum_script::UiScriptHost>),
            _ => None,
        };
        let result = match host {
            Some(host) => self.runner.eval_with_host(&code, &input, host).await,
            None => self.runner.eval_pure(&code, &input).await,
        }
        .map_err(Error::invalid)?;
        Ok(json!({ "result": result }))
    }
}

// ===========================================================================
// External MCP server management (SOUL §26)
// ===========================================================================
//
// `list/create/edit/delete_mcp_server` let an (admin) agent manage external MCP
// server connections at runtime. Definitions persist in `mcp_servers` (§18,
// workspace-scoped); the live tools hot-(dis)connect through the [`McpManager`]
// into the registry's runtime overlay, so a created server is usable in the same
// session and a deleted one's tools vanish at once — no restart.
//
// Gated on the `mcp` domain, deny-by-default (§19): `mcp:read` to list, `mcp:write`
// to create/edit, `mcp:delete` to delete — none held by a base role (admin/owner
// only), like `agent_profile:*`. Managing a server is powerful (it spawns
// processes / makes network egress), so it stays admin-only; using the resulting
// tools is the separate `mcp:use@{server}` gate.
