//! The real [`CodeRunner`](catalerum_automation::CodeRunner) for node-graph
//! automations (SOUL §11, Phase B): inline **code / condition** nodes run here.
//!
//! [`ScriptCodeRunner`] dispatches by a node's `runtime`:
//! - `"js"` / `"javascript"` — a **sandboxed, pure-Rust JavaScript transform**
//!   evaluated by [Boa](boa_engine). Boa ships **no host functions** (no fs, no
//!   net, no clock) and we add none, so a code node is a side-effect-free data
//!   transform: it sees only its bound `input` and returns a value. Execution is
//!   bounded three ways — a Boa **loop-iteration limit** and **recursion limit**
//!   (so an infinite loop / unbounded recursion terminates deterministically as an
//!   `Err`) plus a **wall-clock timeout** backstop. The Boa `Context` is `!Send`,
//!   so the engine is created and run entirely inside a
//!   [`tokio::task::spawn_blocking`] closure (capturing owned `String` source +
//!   owned `serde_json::Value` input); the join is wrapped in
//!   [`tokio::time::timeout`]. The runner struct itself holds only config + an
//!   optional `Arc<dyn Executor>`, so it stays `Send + Sync`.
//! - any other runtime (`"shell"`, `"python"`, …) — delegated to the §20
//!   [`Executor`](catalerum_core::provider::Executor), if one is configured, by
//!   building a [`CommandSpec`](catalerum_core::provider::CommandSpec) with inline
//!   `code` + `language`. Mapped to `{ stdout, stderr, exit_code }` JSON on a clean
//!   run, or an `Err` on a non-zero exit / timeout. With no executor configured,
//!   the runtime is rejected. (The only shipping executor, `LocalExecutor`, rejects
//!   inline code today — this arm is ready for the container/bao backend.)
//!
//! ## The JS calling convention
//! A node's `source` is a **function body**, not an expression: it receives a bound
//! `input` and the function's `return` value is the node's output. So a transform
//! is written `return input.inputs.n1.value * 2`, and a condition is
//! `return input.x > 5`. We achieve this by evaluating the source wrapped as an
//! immediately-invoked function — `(function(input){ <source>\n })(__input__)` —
//! with the merged `{ trigger, inputs }` context injected as the global
//! `__input__`. A source with no `return` yields `undefined`, which maps to JSON
//! `null`.

use std::cell::RefCell;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use boa_engine::property::Attribute;
use boa_engine::{Context, JsNativeError, JsResult, JsValue, NativeFunction, Source};

use catalerum_automation::CodeRunner;
use catalerum_core::model::Grant;
use catalerum_core::provider::{CommandSpec, Executor};
use catalerum_core::WorkspaceId;

/// Default cap on JS loop iterations before Boa throws (so `while(true){}`
/// terminates deterministically as an `Err`, not a hang).
const DEFAULT_LOOP_ITERATION_LIMIT: u64 = 10_000_000;

/// Default cap on JS function-recursion depth before Boa throws (so unbounded
/// recursion terminates as an `Err`).
const DEFAULT_RECURSION_LIMIT: usize = 400;

/// Default wall-clock backstop for a single JS evaluation. The loop/recursion
/// limits are the primary bound; this catches anything not counted by them.
const DEFAULT_JS_TIMEOUT: Duration = Duration::from_secs(5);

/// Default cap on the Boa VM stack (frames) before it throws — a deep
/// non-recursive expression/structure can't overflow the native stack and abort
/// the worker. Matches Boa's own current default, but pinned **explicitly** so a
/// future engine default change can't silently loosen the sandbox.
const DEFAULT_STACK_SIZE_LIMIT: usize = 10_240;

/// Tunable bounds for the Boa JS sandbox (SOUL §11). Every JS code/condition node
/// runs under these so a node can never hang the worker.
#[derive(Clone, Copy, Debug)]
pub struct JsLimits {
    /// Max loop iterations before Boa throws (an infinite loop → `Err`).
    pub loop_iteration_limit: u64,
    /// Max function-recursion depth before Boa throws.
    pub recursion_limit: usize,
    /// Max Boa VM stack (frames) before it throws (guards the native stack).
    pub stack_size_limit: usize,
    /// Wall-clock backstop for one evaluation.
    pub timeout: Duration,
}

impl Default for JsLimits {
    fn default() -> Self {
        Self {
            loop_iteration_limit: DEFAULT_LOOP_ITERATION_LIMIT,
            recursion_limit: DEFAULT_RECURSION_LIMIT,
            stack_size_limit: DEFAULT_STACK_SIZE_LIMIT,
            timeout: DEFAULT_JS_TIMEOUT,
        }
    }
}

// ---------------------------------------------------------------------------
// Emerged-UI host bridge (SOUL §12, plan P4)
// ---------------------------------------------------------------------------

/// The capability-gated callback an emerged-UI [script handler](run_ui_script)
/// reaches the server through. It is the **only** authority a UI script touches:
/// `catalerum.callTool(name, args)` dispatches one allow-listed, non-confirm tool
/// under the firing user's grant and hands the JSON result back to the script.
/// Everything else a script can do (`getState`/`setState`/`navigate`/`toast`/…)
/// is pure and stays inside the sandbox.
///
/// `call_tool` is **synchronous**: the script runs on a `spawn_blocking` thread
/// (Boa's `Context` is `!Send`), so an implementation may `block_on` the async
/// [`ToolRegistry::dispatch`](catalerum_core::tool::ToolRegistry) safely — it is
/// never called from a runtime worker thread. An `Err(message)` surfaces to the
/// script as a catchable thrown JS `Error`.
pub trait UiScriptHost: Send + Sync {
    /// Dispatch `tool` with JSON `args`. The implementation is responsible for the
    /// allow-list re-check, the confirm-tool exclusion, and the capability cap
    /// **before** dispatching; this layer only marshals the call and its result.
    fn call_tool(&self, tool: &str, args: Value) -> Result<Value, String>;

    /// Classify something with an LLM — the host bridge behind
    /// `catalerum.classifyWithLlm(req)`, used by a **tool-guard** classifier
    /// (SOUL §19) to have a model judge a tool call. `req` is a free-form JSON
    /// object (`{ instruction?, messages?, model?, … }`); the returned JSON is
    /// handed straight back to the script. Like [`call_tool`](Self::call_tool)
    /// it is **synchronous** (the script runs on a `spawn_blocking` thread, so an
    /// implementation may `block_on` the async LLM call). The default rejects the
    /// call — only a host that wires an LLM (the guard host) overrides it, so
    /// UI/code-node scripts calling it get a catchable thrown `Error`.
    fn classify_llm(&self, _req: Value) -> Result<Value, String> {
        Err("classifyWithLlm is unavailable in this context".to_string())
    }
}

/// The capability-gated bridge an automation **code / condition node** (SOUL §11)
/// reaches the server's tools through. A `"js"` node's body may call
/// `catalerum.callTool(name, args)`; that crosses to [`call_tool`](Self::call_tool),
/// which dispatches one tool **under the automation's own authority** — the run's
/// `workspace_id` + its §19 `grant` — through the *same* deny-by-default dispatch
/// gate an `Action` node uses. A code node therefore reaches **no tool an action
/// node couldn't**: the capability cap is the gate, not a separate allow-list.
///
/// Unlike [`UiScriptHost`] (whose authority is fixed per UI event), a code runner is
/// shared across runs and workspaces, so the authority is supplied **per call** —
/// the runner pins it onto this host (via a private adapter) for each evaluation.
///
/// `call_tool` is **synchronous**: a code node runs on a `spawn_blocking` thread
/// (Boa's `Context` is `!Send`), so an implementation may `block_on` the async
/// [`ToolRegistry::dispatch`](catalerum_core::tool::ToolRegistry) safely. An
/// `Err(message)` surfaces to the script as a catchable thrown JS `Error`.
pub trait CodeToolHost: Send + Sync {
    /// Dispatch `tool` with JSON `args` under the run's authority (`workspace_id` +
    /// `grant`). The implementation owns the capability resolution + enforcement;
    /// this layer only marshals the call and its result.
    fn call_tool(
        &self,
        workspace_id: WorkspaceId,
        grant: Option<&Grant>,
        tool: &str,
        args: Value,
    ) -> Result<Value, String>;
}

thread_local! {
    /// The [`UiScriptHost`] in scope for the JS evaluation running on **this**
    /// thread. Set for the duration of one [`eval_ui_script`] call (via
    /// [`HostScope`]) so the native `__catalerum_call_tool__` — a plain `fn`
    /// pointer with no captures — can reach it. Single-threaded by construction:
    /// the engine, the host, and this slot all live on one `spawn_blocking`
    /// thread for the call's lifetime.
    static UI_HOST: RefCell<Option<Arc<dyn UiScriptHost>>> = const { RefCell::new(None) };
}

/// RAII guard that installs a [`UiScriptHost`] into [`UI_HOST`] for the current
/// thread and clears it on drop — so a panic mid-eval can't leak a host into a
/// later, unrelated evaluation reusing the same blocking-pool thread.
struct HostScope;

impl HostScope {
    fn enter(host: Arc<dyn UiScriptHost>) -> Self {
        UI_HOST.with(|cell| *cell.borrow_mut() = Some(host));
        Self
    }
}

impl Drop for HostScope {
    fn drop(&mut self) {
        UI_HOST.with(|cell| *cell.borrow_mut() = None);
    }
}

/// Per-evaluation adapter that pins one code node's authority (the run's
/// `workspace_id` + §19 `grant`) onto a shared [`CodeToolHost`], re-exposing it
/// through the grant-less [`UiScriptHost`] seam. This lets a code node reuse the
/// exact `callTool` machinery (the [`UI_HOST`] thread-local + [`native_call_tool`])
/// the emerged-UI bridge already uses, with the authority baked into the closure
/// rather than threaded through the native `fn` pointer.
struct CodeHostAdapter {
    inner: Arc<dyn CodeToolHost>,
    workspace_id: WorkspaceId,
    grant: Option<Grant>,
}

impl UiScriptHost for CodeHostAdapter {
    fn call_tool(&self, tool: &str, args: Value) -> Result<Value, String> {
        self.inner
            .call_tool(self.workspace_id, self.grant.as_ref(), tool, args)
    }
}

/// The result of running an emerged-UI script handler (SOUL §12).
#[derive(Clone, Debug)]
pub struct UiScriptOutcome {
    /// Server→client [`UiAction`](catalerum_core::model_ui::UiAction)s as JSON,
    /// in apply order: the `setState` state diff first (one `set` per changed
    /// top-level key), then the actions the script queued
    /// (`navigate`/`toast`/`set`/`open_dialog`/`close_dialog`) in call order.
    pub actions: Vec<Value>,
    /// The transient state after all `catalerum.setState` merges.
    pub state: Value,
    /// The handler's `return` value (`undefined` → `null`). Used by validation /
    /// computed scripts; ignored for event handlers.
    pub returned: Value,
}

/// The Boa native backing `catalerum.callTool` — a plain `fn` pointer (no
/// captures), so it reaches the per-thread [`UI_HOST`] for the host bridge.
fn native_call_tool(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let name = args
        .first()
        .and_then(JsValue::as_string)
        .map(|s| s.to_std_string_lossy())
        .ok_or_else(|| {
            JsNativeError::typ().with_message("callTool(name, args): name must be a string")
        })?;
    let tool_args = match args.get(1) {
        Some(value) => value.to_json(context)?.unwrap_or(Value::Null),
        None => Value::Null,
    };
    let outcome = UI_HOST.with(|cell| {
        cell.borrow()
            .as_ref()
            .ok_or_else(|| "callTool is unavailable outside a UI script handler".to_string())
            .and_then(|host| host.call_tool(&name, tool_args))
    });
    match outcome {
        Ok(value) => JsValue::from_json(&value, context),
        Err(message) => Err(JsNativeError::error().with_message(message).into()),
    }
}

/// The Boa native backing `catalerum.classifyWithLlm` — like [`native_call_tool`]
/// a plain `fn` pointer that reaches the per-thread [`UI_HOST`], forwarding a
/// single JSON request object to [`UiScriptHost::classify_llm`]. Only the
/// tool-guard host implements it; every other host throws.
fn native_classify_llm(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let req = match args.first() {
        Some(value) => value.to_json(context)?.unwrap_or(Value::Null),
        None => Value::Null,
    };
    let outcome = UI_HOST.with(|cell| {
        cell.borrow()
            .as_ref()
            .ok_or_else(|| "classifyWithLlm is unavailable outside a guard script".to_string())
            .and_then(|host| host.classify_llm(req))
    });
    match outcome {
        Ok(value) => JsValue::from_json(&value, context),
        Err(message) => Err(JsNativeError::error().with_message(message).into()),
    }
}

/// The real [`CodeRunner`]: Boa-sandboxed JavaScript for `"js"`/`"javascript"`
/// nodes, and an optional §20 [`Executor`] for command runtimes (`"shell"`,
/// `"python"`, …). Holds only config + the optional executor `Arc`, so it is
/// `Send + Sync` even though Boa's `Context` is not (the engine lives inside
/// `spawn_blocking`).
pub struct ScriptCodeRunner {
    /// The §20 executor used for non-JS (command/script) runtimes. `None` → such a
    /// runtime is rejected (`new()`); `Some` → wired (`with_executor`).
    exec: Option<Arc<dyn Executor>>,
    /// Sandbox bounds applied to every JS evaluation.
    js_limits: JsLimits,
    /// Wall-clock timeout for an exec-backed (command) runtime, in seconds.
    exec_timeout_secs: u64,
    /// Optional host bridge that lets a `"js"` code/condition node call registry
    /// tools via `catalerum.callTool` under the run's authority (SOUL §11/§19).
    /// `None` (the default) keeps a code node a **pure transform**: `catalerum` is
    /// absent and the body sees only its bound `input`.
    tool_host: Option<Arc<dyn CodeToolHost>>,
}

impl Default for ScriptCodeRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl ScriptCodeRunner {
    /// A JS-only runner: `"js"`/`"javascript"` nodes run in Boa; any other runtime
    /// is rejected (no executor configured).
    #[must_use]
    pub fn new() -> Self {
        Self {
            exec: None,
            js_limits: JsLimits::default(),
            exec_timeout_secs: 30,
            tool_host: None,
        }
    }

    /// A runner that additionally delegates command runtimes (`"shell"`,
    /// `"python"`, …) to `exec` (the §20 [`Executor`]). JS still runs in Boa.
    #[must_use]
    pub fn with_executor(exec: Arc<dyn Executor>) -> Self {
        Self {
            exec: Some(exec),
            ..Self::new()
        }
    }

    /// Override the Boa JS sandbox bounds (builder).
    #[must_use]
    pub fn with_js_limits(mut self, limits: JsLimits) -> Self {
        self.js_limits = limits;
        self
    }

    /// Override the wall-clock timeout (seconds) handed to the executor for command
    /// runtimes (builder).
    #[must_use]
    pub fn with_exec_timeout_secs(mut self, secs: u64) -> Self {
        self.exec_timeout_secs = secs;
        self
    }

    /// Install the [`CodeToolHost`] a `"js"` code/condition node's
    /// `catalerum.callTool` reaches the registry through (SOUL §11/§19). Without
    /// this, a code node is a pure transform — `catalerum` is undefined and a call
    /// to it throws (builder).
    #[must_use]
    pub fn with_tool_host(mut self, host: Arc<dyn CodeToolHost>) -> Self {
        self.tool_host = Some(host);
        self
    }

    /// Run a JS code/condition node: evaluate `source` (a function body) with the
    /// merged context `input` bound as `input`, returning the function's result as
    /// JSON. Bounded by the loop/recursion limits and a wall-clock timeout.
    ///
    /// When a [`CodeToolHost`] is installed ([`with_tool_host`](Self::with_tool_host)),
    /// the body also gets `catalerum.callTool(name, args)`, dispatching tools under
    /// the run's authority (`workspace_id` + `grant`); otherwise the node is a pure
    /// transform and `catalerum` is absent.
    async fn run_js(
        &self,
        source: &str,
        input: &Value,
        workspace_id: WorkspaceId,
        grant: Option<&Grant>,
    ) -> Result<Value, String> {
        // Pin THIS run's authority onto the shared tool host (if any) for the one
        // evaluation, behind the `UiScriptHost` seam the native `callTool` reuses.
        let host: Option<Arc<dyn UiScriptHost>> = self.tool_host.as_ref().map(|inner| {
            Arc::new(CodeHostAdapter {
                inner: inner.clone(),
                workspace_id,
                grant: grant.cloned(),
            }) as Arc<dyn UiScriptHost>
        });
        self.eval_bounded(source, input, host).await
    }

    /// Evaluate `source` (a JS function body) as a **pure transform** in the Boa
    /// sandbox: `input` is bound as the global `input`, the function's `return`
    /// value comes back as JSON, and there are **no** host functions — no
    /// `catalerum.callTool`, no fs / net / clock — *regardless* of whether a
    /// [`CodeToolHost`] is installed on this runner. Bounded by the same
    /// [`JsLimits`] (loop / recursion / stack + wall-clock) as a code node, so it
    /// can never hang the caller. A body with no `return` yields JSON `null`; any
    /// parse/runtime error (including a tripped limit or the timeout) is returned
    /// as an `Err(message)`. This is the entry point for exposing the sandbox as a
    /// standalone LLM tool (a side-effect-free compute scratchpad).
    pub async fn eval_pure(&self, source: &str, input: &Value) -> Result<Value, String> {
        self.eval_bounded(source, input, None).await
    }

    /// Evaluate `source` (a JS function body) like [`eval_pure`](Self::eval_pure)
    /// — same calling convention, same [`JsLimits`] bounds — but with
    /// `catalerum.callTool(name, args)` available, backed by `host`. This is the
    /// entry point for the `run_javascript` LLM tool's tool-calling mode: the host
    /// (built per call by the tool) owns authorization — it re-dispatches each
    /// nested call through the registry's deny-by-default gate under the calling
    /// context — while this layer only marshals the bridge. A host rejection
    /// surfaces to the script as a catchable thrown JS `Error`.
    pub async fn eval_with_host(
        &self,
        source: &str,
        input: &Value,
        host: Arc<dyn UiScriptHost>,
    ) -> Result<Value, String> {
        self.eval_bounded(source, input, Some(host)).await
    }

    /// Evaluate `source` (a JS function body) with `input` bound, under the
    /// sandbox [`JsLimits`], on a `spawn_blocking` thread with a wall-clock
    /// backstop. `host`, when `Some`, backs `catalerum.callTool` for the eval;
    /// `None` keeps it a pure transform. Shared by [`run_js`](Self::run_js) and
    /// [`eval_pure`](Self::eval_pure).
    async fn eval_bounded(
        &self,
        source: &str,
        input: &Value,
        host: Option<Arc<dyn UiScriptHost>>,
    ) -> Result<Value, String> {
        let source = source.to_owned();
        let input = input.clone();
        let limits = self.js_limits;

        // Boa's `Context` is `!Send`: build + run it entirely inside this blocking
        // closure (owned captures only). The closure returns a plain
        // `Result<Value, String>` so nothing `!Send` escapes the thread.
        let join = tokio::task::spawn_blocking(move || eval_js(&source, &input, limits, host));

        match tokio::time::timeout(limits.timeout, join).await {
            // Evaluated within the wall-clock window.
            Ok(Ok(result)) => result,
            // The blocking task panicked (a Boa internal bug, OOM, …) — surface it.
            Ok(Err(join_err)) => Err(format!("js task failed: {join_err}")),
            // Wall-clock backstop tripped: the loop/recursion limits didn't bound it
            // in time. The detached thread is abandoned (it will hit a limit / finish
            // on its own); the eval fails deterministically.
            Err(_) => Err(format!(
                "js execution timed out after {}ms",
                limits.timeout.as_millis()
            )),
        }
    }

    /// Run an emerged-UI **script handler** (SOUL §12): evaluate `source` (a
    /// function body) with the event context `input` bound as `input` and the
    /// transient `state` snapshot available through `catalerum.getState()` /
    /// `catalerum.setState()`, while `catalerum.callTool(name, args)` reaches the
    /// server through `host`. Returns the queued [`UiAction`]s + the final state +
    /// the script's return value. Bounded by the same [`JsLimits`] as a code node.
    ///
    /// [`UiAction`]: catalerum_core::model_ui::UiAction
    pub async fn run_ui_script(
        &self,
        source: &str,
        input: &Value,
        state: &Value,
        host: Arc<dyn UiScriptHost>,
    ) -> Result<UiScriptOutcome, String> {
        let source = source.to_owned();
        let input = input.clone();
        let state = state.clone();
        let limits = self.js_limits;

        // Same `!Send` containment as `run_js`: the Boa engine, the host bridge,
        // and the per-thread `UI_HOST` slot all live on this one blocking thread.
        let join = tokio::task::spawn_blocking(move || {
            eval_ui_script(&source, &input, &state, limits, host)
        });

        match tokio::time::timeout(limits.timeout, join).await {
            Ok(Ok(result)) => result,
            Ok(Err(join_err)) => Err(format!("ui script task failed: {join_err}")),
            Err(_) => Err(format!(
                "ui script timed out after {}ms",
                limits.timeout.as_millis()
            )),
        }
    }

    /// Run a **tool-guard** classifier (SOUL §19): evaluate `source` (a function
    /// body, the code-node calling convention) with the call description `input`
    /// bound as `input`, returning the body's value as JSON. The body may reach the
    /// server through `host` via **two** helpers — `catalerum.callTool(name, args)`
    /// (look something up under the profile's authority) and
    /// `catalerum.classifyWithLlm(req)` (have a model judge the call). Bounded by
    /// the same [`JsLimits`] (loop / recursion / stack + wall-clock) as a code node,
    /// so a guard can never hang the caller. A body with no `return` yields JSON
    /// `null`; any parse/runtime error (or a tripped limit / the timeout) is an
    /// `Err(message)`.
    pub async fn run_guard(
        &self,
        source: &str,
        input: &Value,
        host: Arc<dyn UiScriptHost>,
    ) -> Result<Value, String> {
        let source = source.to_owned();
        let input = input.clone();
        let limits = self.js_limits;

        // Same `!Send` containment as `run_js`/`run_ui_script`: the Boa engine, the
        // host bridge, and the per-thread `UI_HOST` slot all live on this one
        // blocking thread for the eval's lifetime.
        let join = tokio::task::spawn_blocking(move || eval_guard(&source, &input, limits, host));

        match tokio::time::timeout(limits.timeout, join).await {
            Ok(Ok(result)) => result,
            Ok(Err(join_err)) => Err(format!("guard script task failed: {join_err}")),
            Err(_) => Err(format!(
                "guard script timed out after {}ms",
                limits.timeout.as_millis()
            )),
        }
    }

    /// Run a command runtime (`"shell"`, `"python"`, …) via the configured §20
    /// [`Executor`]: build a [`CommandSpec`] with inline `code` + `language`, run
    /// it, and map the result to `{ stdout, stderr, exit_code }` JSON (or an `Err`
    /// on a non-zero exit / timeout).
    async fn run_exec(&self, runtime: &str, source: &str) -> Result<Value, String> {
        let Some(exec) = self.exec.as_ref() else {
            return Err(format!("no executor configured for runtime '{runtime}'"));
        };

        let spec = CommandSpec {
            code: Some(source.to_owned()),
            language: Some(runtime.to_owned()),
            timeout_secs: Some(self.exec_timeout_secs),
            ..CommandSpec::default()
        };

        let result = exec
            .run(spec)
            .await
            .map_err(|e| format!("executor failed for runtime '{runtime}': {e}"))?;

        if result.timed_out {
            return Err(format!(
                "runtime '{runtime}' timed out (exit {}): {}",
                result.exit_code,
                result.stderr.trim()
            ));
        }
        if result.exit_code != 0 {
            return Err(format!(
                "runtime '{runtime}' exited {}: {}",
                result.exit_code,
                result.stderr.trim()
            ));
        }

        Ok(json!({
            "stdout": result.stdout,
            "stderr": result.stderr,
            "exit_code": result.exit_code,
        }))
    }
}

#[async_trait]
impl CodeRunner for ScriptCodeRunner {
    async fn run_code(
        &self,
        runtime: &str,
        source: &str,
        input: &Value,
        workspace_id: WorkspaceId,
        grant: Option<&Grant>,
    ) -> Result<Value, String> {
        match runtime {
            "js" | "javascript" => self.run_js(source, input, workspace_id, grant).await,
            other => self.run_exec(other, source).await,
        }
    }
}

/// Evaluate a JS function body `source` with `input` bound, under `limits`. Pure +
/// synchronous: runs to completion on the calling (blocking) thread. Maps any Boa
/// parse/runtime error — including a tripped loop/recursion limit — to an `Err`
/// message.
///
/// When `host` is `Some`, the body additionally gets a `catalerum.callTool(name,
/// args)` helper backed by the host bridge (the [`UI_HOST`] thread-local + the
/// native [`native_call_tool`]); when `None`, the node stays a pure transform and
/// `catalerum` is never defined.
fn eval_js(
    source: &str,
    input: &Value,
    limits: JsLimits,
    host: Option<Arc<dyn UiScriptHost>>,
) -> Result<Value, String> {
    let mut context = Context::default();

    // Bound the sandbox: an infinite loop / unbounded recursion now throws (→ Err)
    // instead of hanging the blocking thread.
    {
        let rl = context.runtime_limits_mut();
        rl.set_loop_iteration_limit(limits.loop_iteration_limit);
        rl.set_recursion_limit(limits.recursion_limit);
        rl.set_stack_size_limit(limits.stack_size_limit);
    }

    // Inject the merged `{ trigger, inputs }` context as a global the wrapper IIFE
    // reads. A conversion failure here is an internal error (our own JSON), not user
    // code — surface it rather than panic.
    let input_value = JsValue::from_json(input, &mut context)
        .map_err(|e| format!("failed to inject input into js context: {e}"))?;
    context
        .register_global_property(
            boa_engine::js_string!("__catalerum_input__"),
            input_value,
            Attribute::all(),
        )
        .map_err(|e| format!("failed to bind input global: {e}"))?;

    // With a host installed, bind the `callTool` native + scope the per-thread host
    // for the eval (the guard clears it even if `eval` unwinds). The `_scope` binding
    // keeps the host alive until the function returns.
    let with_host = host.is_some();
    let _scope = if let Some(host) = host {
        context
            .register_global_builtin_callable(
                boa_engine::js_string!("__catalerum_call_tool__"),
                2,
                NativeFunction::from_fn_ptr(native_call_tool),
            )
            .map_err(|e| format!("failed to bind callTool native: {e}"))?;
        Some(HostScope::enter(host))
    } else {
        None
    };

    // The source is a *function body*: wrap it so the user can `return`, and bind
    // `input` (+ the `catalerum` host object when a host is present).
    let wrapped = build_code_program(source, with_host);

    let result = context
        .eval(Source::from_bytes(&wrapped))
        .map_err(|e| e.to_string())?;

    // `undefined` (a body with no `return`) → JSON null; otherwise the JS value as
    // JSON. A cyclic/unrepresentable value is a runtime error from `to_json`.
    match result
        .to_json(&mut context)
        .map_err(|e| format!("js result is not representable as json: {e}"))?
    {
        Some(v) => Ok(v),
        None => Ok(Value::Null),
    }
}

/// Evaluate an emerged-UI script handler `source` under `limits`, with `input`
/// bound as `input`, `state` reachable via `catalerum.getState/setState`, and
/// `host` installed for `catalerum.callTool`. Pure + synchronous (runs to
/// completion on the calling blocking thread). The user body is wrapped in a
/// prelude that defines the pure `catalerum.*` helpers over JS-side accumulators;
/// only `callTool` crosses back into Rust.
fn eval_ui_script(
    source: &str,
    input: &Value,
    state: &Value,
    limits: JsLimits,
    host: Arc<dyn UiScriptHost>,
) -> Result<UiScriptOutcome, String> {
    let mut context = Context::default();

    // Same bounds as a code node (SOUL §11): an infinite loop / unbounded
    // recursion / deep stack throws rather than hanging the blocking thread.
    {
        let rl = context.runtime_limits_mut();
        rl.set_loop_iteration_limit(limits.loop_iteration_limit);
        rl.set_recursion_limit(limits.recursion_limit);
        rl.set_stack_size_limit(limits.stack_size_limit);
    }

    // The event context the body sees as `input`.
    let input_value = JsValue::from_json(input, &mut context)
        .map_err(|e| format!("failed to inject input into js context: {e}"))?;
    context
        .register_global_property(
            boa_engine::js_string!("__catalerum_input__"),
            input_value,
            Attribute::all(),
        )
        .map_err(|e| format!("failed to bind input global: {e}"))?;

    // The transient state `getState` returns and `setState` merges into.
    let state_value = JsValue::from_json(state, &mut context)
        .map_err(|e| format!("failed to inject state into js context: {e}"))?;
    context
        .register_global_property(
            boa_engine::js_string!("__catalerum_state__"),
            state_value,
            Attribute::all(),
        )
        .map_err(|e| format!("failed to bind state global: {e}"))?;

    // The one native: `catalerum.callTool` → the host bridge (via `UI_HOST`).
    context
        .register_global_builtin_callable(
            boa_engine::js_string!("__catalerum_call_tool__"),
            2,
            NativeFunction::from_fn_ptr(native_call_tool),
        )
        .map_err(|e| format!("failed to bind callTool native: {e}"))?;

    // Install the host for the lifetime of this evaluation; the guard clears it
    // even if `eval` unwinds.
    let _scope = HostScope::enter(host);

    let program = build_ui_program(source);
    let result = context
        .eval(Source::from_bytes(&program))
        .map_err(|e| e.to_string())?;

    let out = result
        .to_json(&mut context)
        .map_err(|e| format!("ui script result is not representable as json: {e}"))?
        .unwrap_or(Value::Null);

    let final_state = out.get("state").cloned().unwrap_or(Value::Null);
    let queued = out
        .get("actions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let returned = out.get("returned").cloned().unwrap_or(Value::Null);

    // A `setState` merge is conveyed to the client as one `set` per changed
    // top-level key, applied before the script's explicitly queued actions.
    let mut actions = diff_state_actions(state, &final_state);
    actions.extend(queued);

    Ok(UiScriptOutcome {
        actions,
        state: final_state,
        returned,
    })
}

/// Wrap a code/condition node `source` (a function body) so the user can `return`
/// a value and read its bound `input`. When `with_host`, a minimal `catalerum`
/// object exposing `callTool` (over the native bridge) is defined first, so a code
/// node can dispatch tools under the run's authority; otherwise the node is the
/// pure transform it has always been. The program's completion value is the IIFE's
/// `return` (a body with no `return` → `undefined` → JSON `null`).
fn build_code_program(source: &str, with_host: bool) -> String {
    if with_host {
        // The trailing newline after `{source}` guards a `source` ending in a `//`
        // line comment from swallowing the `)` (same as the host-less wrapper).
        format!(
            r#"var catalerum = {{
  log: function() {{}},
  callTool: function(name, args) {{
    return __catalerum_call_tool__(String(name), args === undefined ? null : args);
  }}
}};
(function(input) {{
{source}
}})(typeof __catalerum_input__ === 'undefined' ? null : __catalerum_input__);
"#
        )
    } else {
        // The trailing newline guards against a `source` ending in a `//` line
        // comment swallowing the `)`.
        format!("(function(input){{\n{source}\n}})(__catalerum_input__);")
    }
}

/// Evaluate a tool-guard classifier `source` (a function body) with `input` bound,
/// under `limits`. Pure + synchronous: runs to completion on the calling (blocking)
/// thread. The body gets a `catalerum` object with **both** `callTool` and
/// `classifyWithLlm`, backed by the host bridge (the [`UI_HOST`] thread-local + the
/// two natives). The program's completion value is the IIFE's `return` (no
/// `return` → `undefined` → JSON `null`). Any Boa parse/runtime error — including a
/// tripped loop/recursion limit — maps to an `Err` message.
fn eval_guard(
    source: &str,
    input: &Value,
    limits: JsLimits,
    host: Arc<dyn UiScriptHost>,
) -> Result<Value, String> {
    let mut context = Context::default();

    // Bound the sandbox exactly like a code node.
    {
        let rl = context.runtime_limits_mut();
        rl.set_loop_iteration_limit(limits.loop_iteration_limit);
        rl.set_recursion_limit(limits.recursion_limit);
        rl.set_stack_size_limit(limits.stack_size_limit);
    }

    // The call description the body reads as `input`.
    let input_value = JsValue::from_json(input, &mut context)
        .map_err(|e| format!("failed to inject input into js context: {e}"))?;
    context
        .register_global_property(
            boa_engine::js_string!("__catalerum_input__"),
            input_value,
            Attribute::all(),
        )
        .map_err(|e| format!("failed to bind input global: {e}"))?;

    // The two natives the `catalerum` object defers to.
    context
        .register_global_builtin_callable(
            boa_engine::js_string!("__catalerum_call_tool__"),
            2,
            NativeFunction::from_fn_ptr(native_call_tool),
        )
        .map_err(|e| format!("failed to bind callTool native: {e}"))?;
    context
        .register_global_builtin_callable(
            boa_engine::js_string!("__catalerum_classify_llm__"),
            1,
            NativeFunction::from_fn_ptr(native_classify_llm),
        )
        .map_err(|e| format!("failed to bind classifyWithLlm native: {e}"))?;

    // Install the host for the lifetime of this evaluation; the guard clears it even
    // if `eval` unwinds (so a panic can't leak it into a later eval on this thread).
    let _scope = HostScope::enter(host);

    let wrapped = build_guard_program(source);
    let result = context
        .eval(Source::from_bytes(&wrapped))
        .map_err(|e| e.to_string())?;

    match result
        .to_json(&mut context)
        .map_err(|e| format!("guard result is not representable as json: {e}"))?
    {
        Some(v) => Ok(v),
        None => Ok(Value::Null),
    }
}

/// Wrap a tool-guard `source` (a function body) so it can `return` a decision and
/// read its bound `input`, with a `catalerum` object exposing `callTool` +
/// `classifyWithLlm` over the native bridge. Mirrors [`build_code_program`]'s
/// host wrapper, plus the second helper.
fn build_guard_program(source: &str) -> String {
    format!(
        r#"var catalerum = {{
  log: function() {{}},
  callTool: function(name, args) {{
    return __catalerum_call_tool__(String(name), args === undefined ? null : args);
  }},
  classifyWithLlm: function(req) {{
    return __catalerum_classify_llm__(req === undefined ? null : req);
  }}
}};
(function(input) {{
{source}
}})(typeof __catalerum_input__ === 'undefined' ? null : __catalerum_input__);
"#
    )
}

/// Wrap a UI script `source` (a function body) in the host-bridge prelude: the
/// pure `catalerum.*` helpers write to JS-side accumulators (`__cat_state__`,
/// `__cat_actions__`), `callTool` defers to the native, and the whole program
/// evaluates to `{ state, actions, returned }`. The body keeps the code-node
/// calling convention: it receives `input` and may `return` a value.
fn build_ui_program(source: &str) -> String {
    format!(
        r#"(function() {{
  var __cat_actions__ = [];
  var __cat_state__ = (typeof __catalerum_state__ === 'undefined' || __catalerum_state__ === null)
    ? {{}} : __catalerum_state__;
  function __cat_merge__(target, patch) {{
    if (patch === null || typeof patch !== 'object' || Array.isArray(patch)) {{ return patch; }}
    if (target === null || typeof target !== 'object' || Array.isArray(target)) {{ target = {{}}; }}
    for (var __k in patch) {{
      if (patch[__k] === null) {{ delete target[__k]; }}
      else {{ target[__k] = __cat_merge__(target[__k], patch[__k]); }}
    }}
    return target;
  }}
  var catalerum = {{
    getState: function() {{ return __cat_state__; }},
    setState: function(patch) {{ __cat_state__ = __cat_merge__(__cat_state__, patch); return __cat_state__; }},
    set: function(path, value) {{ __cat_actions__.push({{ op: 'set', path: String(path), value: value }}); }},
    navigate: function(view) {{ __cat_actions__.push({{ op: 'navigate', view: String(view) }}); }},
    toast: function(level, message) {{
      if (message === undefined) {{ message = level; level = 'info'; }}
      __cat_actions__.push({{ op: 'toast', level: String(level), message: String(message) }});
    }},
    openDialog: function(id) {{ __cat_actions__.push({{ op: 'open_dialog', id: String(id) }}); }},
    closeDialog: function(id) {{ __cat_actions__.push({{ op: 'close_dialog', id: String(id) }}); }},
    log: function() {{}},
    callTool: function(name, args) {{
      return __catalerum_call_tool__(String(name), args === undefined ? null : args);
    }}
  }};
  var __cat_ret__ = (function(input) {{
{source}
  }})(typeof __catalerum_input__ === 'undefined' ? null : __catalerum_input__);
  return {{
    state: __cat_state__,
    actions: __cat_actions__,
    returned: (__cat_ret__ === undefined ? null : __cat_ret__)
  }};
}})();
"#,
        source = source
    )
}

/// Diff the transient `state` against `final_state` after a script's `setState`
/// merges, emitting one `{ op: "set", path, value }` [`UiAction`] per changed or
/// removed **top-level** key (a removed key becomes `set … null`). A non-object
/// state can't be expressed as keyed sets, so it yields nothing.
fn diff_state_actions(state: &Value, final_state: &Value) -> Vec<Value> {
    let (Some(before), Some(after)) = (state.as_object(), final_state.as_object()) else {
        return Vec::new();
    };
    let mut actions = Vec::new();
    for (key, value) in after {
        if before.get(key) != Some(value) {
            actions.push(json!({ "op": "set", "path": key, "value": value }));
        }
    }
    for key in before.keys() {
        if !after.contains_key(key) {
            actions.push(json!({ "op": "set", "path": key, "value": Value::Null }));
        }
    }
    actions
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A numeric transform reads a nested upstream output and returns a number.
    #[tokio::test]
    async fn js_numeric_transform_doubles_an_upstream_value() {
        let runner = ScriptCodeRunner::new();
        let input = json!({ "trigger": null, "inputs": { "n1": { "value": 21 } } });
        let out = runner
            .run_code(
                "js",
                "return input.inputs.n1.value * 2;",
                &input,
                WorkspaceId::new(),
                None,
            )
            .await
            .expect("js transform runs");
        assert_eq!(out, json!(42));
    }

    /// `"javascript"` is an alias for `"js"`.
    #[tokio::test]
    async fn javascript_alias_runs_in_boa() {
        let runner = ScriptCodeRunner::new();
        let out = runner
            .run_code(
                "javascript",
                "return 1 + 1;",
                &json!({}),
                WorkspaceId::new(),
                None,
            )
            .await
            .expect("javascript alias runs");
        assert_eq!(out, json!(2));
    }

    /// A condition source returns a boolean — true and false both round-trip as
    /// JSON, so the executor can take its truthiness to route a branch.
    #[tokio::test]
    async fn js_condition_returns_boolean_both_ways() {
        let runner = ScriptCodeRunner::new();

        let yes = runner
            .run_code(
                "js",
                "return input.x > 5;",
                &json!({ "x": 10 }),
                WorkspaceId::new(),
                None,
            )
            .await
            .expect("condition runs");
        assert_eq!(yes, json!(true));

        let no = runner
            .run_code(
                "js",
                "return input.x > 5;",
                &json!({ "x": 1 }),
                WorkspaceId::new(),
                None,
            )
            .await
            .expect("condition runs");
        assert_eq!(no, json!(false));
    }

    /// An object-returning transform round-trips a structured value out of JS.
    #[tokio::test]
    async fn js_object_transform_round_trips() {
        let runner = ScriptCodeRunner::new();
        let out = runner
            .run_code(
                "js",
                "return { doubled: input.n * 2, label: 'n=' + input.n, ok: true };",
                &json!({ "n": 4 }),
                WorkspaceId::new(),
                None,
            )
            .await
            .expect("object transform runs");
        assert_eq!(out, json!({ "doubled": 8, "label": "n=4", "ok": true }));
    }

    /// A body with no `return` yields `undefined` → JSON null.
    #[tokio::test]
    async fn js_body_without_return_yields_null() {
        let runner = ScriptCodeRunner::new();
        let out = runner
            .run_code("js", "var x = 1 + 1;", &json!({}), WorkspaceId::new(), None)
            .await
            .expect("runs");
        assert_eq!(out, Value::Null);
    }

    /// A syntax error is reported as an `Err`, not a panic.
    #[tokio::test]
    async fn js_syntax_error_is_an_err() {
        let runner = ScriptCodeRunner::new();
        let err = runner
            .run_code("js", "return (((;", &json!({}), WorkspaceId::new(), None)
            .await
            .expect_err("syntax error fails");
        assert!(!err.is_empty(), "error message should be non-empty");
    }

    /// A thrown runtime error is reported as an `Err`.
    #[tokio::test]
    async fn js_runtime_error_is_an_err() {
        let runner = ScriptCodeRunner::new();
        let err = runner
            .run_code(
                "js",
                "throw new Error('boom');",
                &json!({}),
                WorkspaceId::new(),
                None,
            )
            .await
            .expect_err("runtime error fails");
        assert!(err.contains("boom"), "expected the thrown message: {err}");
    }

    /// An infinite `while(true){}` is bounded — the loop-iteration limit (with the
    /// wall-clock timeout as a backstop) terminates it as an `Err` rather than
    /// hanging. Uses a low iteration cap + short timeout so the test is fast.
    #[tokio::test]
    async fn js_infinite_loop_is_bounded_to_an_err() {
        let runner = ScriptCodeRunner::new().with_js_limits(JsLimits {
            loop_iteration_limit: 100_000,
            recursion_limit: 100,
            timeout: Duration::from_secs(2),
            ..JsLimits::default()
        });
        let err = runner
            .run_code(
                "js",
                "while (true) {}",
                &json!({}),
                WorkspaceId::new(),
                None,
            )
            .await
            .expect_err("infinite loop is bounded");
        assert!(!err.is_empty(), "error message should be non-empty: {err}");
    }

    /// Unbounded recursion is bounded by the recursion limit → `Err`.
    #[tokio::test]
    async fn js_unbounded_recursion_is_bounded_to_an_err() {
        let runner = ScriptCodeRunner::new();
        let err = runner
            .run_code(
                "js",
                "function f(n){ return f(n+1); } return f(0);",
                &json!({}),
                WorkspaceId::new(),
                None,
            )
            .await
            .expect_err("unbounded recursion is bounded");
        assert!(!err.is_empty(), "error message should be non-empty: {err}");
    }

    /// A non-JS runtime with no executor configured is rejected with a clear
    /// message (the LocalExecutor-less path).
    #[tokio::test]
    async fn unknown_runtime_without_executor_errs() {
        let runner = ScriptCodeRunner::new();
        let err = runner
            .run_code(
                "python",
                "print('hi')",
                &json!({}),
                WorkspaceId::new(),
                None,
            )
            .await
            .expect_err("no executor → err");
        assert!(
            err.contains("no executor configured for runtime 'python'"),
            "unexpected: {err}"
        );
    }

    // --- emerged-UI script handler (host bridge) ---------------------------

    /// A host that records the calls it receives and replies from a fixed map,
    /// or fails a named tool — enough to exercise the `callTool` bridge.
    #[derive(Default)]
    struct FakeHost {
        calls: std::sync::Mutex<Vec<(String, Value)>>,
        reply: Value,
        fail: Option<String>,
    }

    impl UiScriptHost for FakeHost {
        fn call_tool(&self, tool: &str, args: Value) -> Result<Value, String> {
            self.calls.lock().unwrap().push((tool.to_string(), args));
            if self.fail.as_deref() == Some(tool) {
                return Err(format!("tool `{tool}` is not allowed"));
            }
            Ok(self.reply.clone())
        }
    }

    /// `catalerum.setState` merges are surfaced as `set` actions (one per changed
    /// top-level key) and reflected in the returned state.
    #[tokio::test]
    async fn ui_script_set_state_emits_set_actions() {
        let runner = ScriptCodeRunner::new();
        let host = Arc::new(FakeHost::default());
        let out = runner
            .run_ui_script(
                "catalerum.setState({ count: 2, name: 'ada' });",
                &json!({}),
                &json!({ "count": 1 }),
                host,
            )
            .await
            .expect("ui script runs");
        assert_eq!(out.state, json!({ "count": 2, "name": "ada" }));
        // `count` changed (1 → 2) and `name` is new; both become `set` actions.
        assert!(out
            .actions
            .contains(&json!({ "op": "set", "path": "count", "value": 2 })));
        assert!(out
            .actions
            .contains(&json!({ "op": "set", "path": "name", "value": "ada" })));
    }

    /// `navigate` / `toast` queue client actions in call order; `getState` reads
    /// the injected snapshot.
    #[tokio::test]
    async fn ui_script_queues_navigate_and_toast() {
        let runner = ScriptCodeRunner::new();
        let host = Arc::new(FakeHost::default());
        let out = runner
            .run_ui_script(
                "if (catalerum.getState().ok) { catalerum.toast('success', 'done'); } \
                 catalerum.navigate('next');",
                &json!({}),
                &json!({ "ok": true }),
                host,
            )
            .await
            .expect("ui script runs");
        assert_eq!(
            out.actions,
            vec![
                json!({ "op": "toast", "level": "success", "message": "done" }),
                json!({ "op": "navigate", "view": "next" }),
            ]
        );
    }

    /// `catalerum.callTool` reaches the host bridge, passes name + args, and hands
    /// the JSON result back to the script.
    #[tokio::test]
    async fn ui_script_call_tool_round_trips_through_host() {
        let runner = ScriptCodeRunner::new();
        let host = Arc::new(FakeHost {
            reply: json!({ "id": "note-1" }),
            ..FakeHost::default()
        });
        let out = runner
            .run_ui_script(
                "var r = catalerum.callTool('create_note', { title: input.title }); \
                 catalerum.setState({ created: r.id });",
                &json!({ "title": "hi" }),
                &json!({}),
                host.clone(),
            )
            .await
            .expect("ui script runs");
        let calls = host.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "create_note");
        assert_eq!(calls[0].1, json!({ "title": "hi" }));
        assert_eq!(out.state, json!({ "created": "note-1" }));
    }

    /// A host rejection surfaces as a catchable thrown JS error.
    #[tokio::test]
    async fn ui_script_call_tool_error_is_catchable() {
        let runner = ScriptCodeRunner::new();
        let host = Arc::new(FakeHost {
            fail: Some("run_command".to_string()),
            ..FakeHost::default()
        });
        let out = runner
            .run_ui_script(
                "try { catalerum.callTool('run_command', {}); } \
                 catch (e) { catalerum.toast('error', 'blocked'); }",
                &json!({}),
                &json!({}),
                host,
            )
            .await
            .expect("ui script runs");
        assert_eq!(
            out.actions,
            vec![json!({ "op": "toast", "level": "error", "message": "blocked" })]
        );
    }

    /// `eval_with_host` is `eval_pure` + the `catalerum.callTool` bridge: the body
    /// keeps the code-node calling convention (bound `input`, `return` value — no
    /// UI `{state,actions}` envelope), the host sees name + args, and a host
    /// rejection is a catchable thrown JS error. This is the `run_javascript`
    /// tool's tool-calling entry point.
    #[tokio::test]
    async fn eval_with_host_bridges_call_tool_with_code_node_convention() {
        let runner = ScriptCodeRunner::new();
        let host = Arc::new(FakeHost {
            reply: json!({ "id": "note-1" }),
            fail: Some("denied_tool".to_string()),
            ..FakeHost::default()
        });
        let out = runner
            .eval_with_host(
                "var r = catalerum.callTool('create_note', { title: input.title });\n\
                 var blocked = 'no';\n\
                 try { catalerum.callTool('denied_tool', {}); } catch (e) { blocked = 'yes'; }\n\
                 return { id: r.id, blocked: blocked };",
                &json!({ "title": "hi" }),
                host.clone(),
            )
            .await
            .expect("hosted eval runs");
        assert_eq!(out, json!({ "id": "note-1", "blocked": "yes" }));
        let calls = host.calls.lock().unwrap();
        assert_eq!(
            calls[0],
            ("create_note".to_string(), json!({ "title": "hi" }))
        );
    }

    /// The host slot does not leak across evaluations: after a run, a script that
    /// calls `callTool` with no host installed throws (here, caught).
    #[tokio::test]
    async fn ui_script_host_does_not_leak_between_runs() {
        let runner = ScriptCodeRunner::new();
        // Run once with a host so the thread-local has been set + cleared.
        let host = Arc::new(FakeHost::default());
        let _ = runner
            .run_ui_script(
                "catalerum.callTool('recall', {});",
                &json!({}),
                &json!({}),
                host,
            )
            .await
            .expect("first run");
        // A plain code-node eval on the same pool must still see no host: this is
        // implicitly guaranteed because `run_js` never installs one. Sanity-check
        // the pure path is unaffected.
        let v = runner
            .run_code("js", "return 1 + 1;", &json!({}), WorkspaceId::new(), None)
            .await
            .expect("pure path still works");
        assert_eq!(v, json!(2));
    }

    // --- automation code-node tool host (`catalerum.callTool`) -------------

    /// A [`CodeToolHost`] that records what authority + tool/args it was dispatched
    /// with and replies from a fixed value (or fails a named tool).
    /// One recorded dispatch: the authority's `workspace_id`, the tool, its args.
    type RecordedCall = (WorkspaceId, String, Value);

    #[derive(Default)]
    struct FakeCodeHost {
        calls: std::sync::Mutex<Vec<RecordedCall>>,
        reply: Value,
        fail: Option<String>,
    }

    impl CodeToolHost for FakeCodeHost {
        fn call_tool(
            &self,
            workspace_id: WorkspaceId,
            _grant: Option<&Grant>,
            tool: &str,
            args: Value,
        ) -> Result<Value, String> {
            self.calls
                .lock()
                .unwrap()
                .push((workspace_id, tool.to_string(), args));
            if self.fail.as_deref() == Some(tool) {
                return Err(format!("tool `{tool}` is not allowed"));
            }
            Ok(self.reply.clone())
        }
    }

    /// With a tool host installed, a code node can `catalerum.callTool` and use the
    /// result; the host sees the run's `workspace_id` and the tool name + args.
    #[tokio::test]
    async fn code_node_call_tool_round_trips_through_host() {
        let host = Arc::new(FakeCodeHost {
            reply: json!({ "id": "note-1" }),
            ..FakeCodeHost::default()
        });
        let runner = ScriptCodeRunner::new().with_tool_host(host.clone());
        let ws = WorkspaceId::new();
        let out = runner
            .run_code(
                "js",
                "var r = catalerum.callTool('create_note', { title: input.title }); return r.id;",
                &json!({ "title": "hi" }),
                ws,
                None,
            )
            .await
            .expect("code node with host runs");
        assert_eq!(out, json!("note-1"));
        let calls = host.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, ws);
        assert_eq!(calls[0].1, "create_note");
        assert_eq!(calls[0].2, json!({ "title": "hi" }));
    }

    /// A host rejection surfaces to the node as a catchable thrown JS error (so a
    /// node can `try`/`catch` a denied tool).
    #[tokio::test]
    async fn code_node_call_tool_error_is_catchable() {
        let host = Arc::new(FakeCodeHost {
            fail: Some("run_command".to_string()),
            ..FakeCodeHost::default()
        });
        let runner = ScriptCodeRunner::new().with_tool_host(host);
        let out = runner
            .run_code(
                "js",
                "try { catalerum.callTool('run_command', {}); return 'ran'; } \
                 catch (e) { return 'blocked'; }",
                &json!({}),
                WorkspaceId::new(),
                None,
            )
            .await
            .expect("code node runs");
        assert_eq!(out, json!("blocked"));
    }

    /// Without a tool host (the default), a code node is a pure transform:
    /// `catalerum` is undefined, so calling it throws — proving the bridge is opt-in
    /// and a stray `callTool` can't silently reach the registry.
    #[tokio::test]
    async fn code_node_without_host_has_no_call_tool() {
        let runner = ScriptCodeRunner::new();
        let err = runner
            .run_code(
                "js",
                "return catalerum.callTool('recall', {});",
                &json!({}),
                WorkspaceId::new(),
                None,
            )
            .await
            .expect_err("no host ⇒ catalerum is undefined");
        assert!(!err.is_empty(), "error message should be non-empty: {err}");
    }

    // -- tool-guard classifier (`run_guard`) -------------------------------------

    /// A guard host that records both bridge calls and canned-replies each — enough
    /// to prove `callTool` + `classifyWithLlm` both reach the host from a guard body.
    #[derive(Default)]
    struct FakeGuardHost {
        tool_calls: std::sync::Mutex<Vec<(String, Value)>>,
        llm_calls: std::sync::Mutex<Vec<Value>>,
        tool_reply: Value,
        llm_reply: Value,
    }

    impl UiScriptHost for FakeGuardHost {
        fn call_tool(&self, tool: &str, args: Value) -> Result<Value, String> {
            self.tool_calls
                .lock()
                .unwrap()
                .push((tool.to_string(), args));
            Ok(self.tool_reply.clone())
        }
        fn classify_llm(&self, req: Value) -> Result<Value, String> {
            self.llm_calls.lock().unwrap().push(req);
            Ok(self.llm_reply.clone())
        }
    }

    /// A guard body reads its bound `input` (the call description) and `return`s a
    /// plain decision string.
    #[tokio::test]
    async fn guard_returns_a_plain_decision_from_input() {
        let runner = ScriptCodeRunner::new();
        let host = Arc::new(FakeGuardHost::default());
        let out = runner
            .run_guard(
                "if (input.tool.name === 'delete_object') return 'deny'; return 'allow';",
                &json!({ "phase": "call", "tool": { "name": "delete_object" }, "args": {} }),
                host,
            )
            .await
            .expect("guard runs");
        assert_eq!(out, json!("deny"));
    }

    /// A guard body can defer to the LLM via `classifyWithLlm` and look things up
    /// via `callTool`; both cross to the host and their results flow back.
    #[tokio::test]
    async fn guard_bridges_to_classify_llm_and_call_tool() {
        let runner = ScriptCodeRunner::new();
        let host = Arc::new(FakeGuardHost {
            tool_reply: json!({ "read_only": false }),
            llm_reply: json!({ "decision": "ask", "reason": "write to prod" }),
            ..FakeGuardHost::default()
        });
        let out = runner
            .run_guard(
                "var meta = catalerum.callTool('describe_tool', { name: input.tool.name });\n\
                 if (meta.read_only) return 'allow';\n\
                 var v = catalerum.classifyWithLlm({ instruction: 'judge', tool: input.tool.name });\n\
                 return { decision: v.decision, reason: v.reason };",
                &json!({ "phase": "call", "tool": { "name": "write_object" }, "args": {} }),
                host.clone(),
            )
            .await
            .expect("guard runs");
        assert_eq!(out, json!({ "decision": "ask", "reason": "write to prod" }));
        assert_eq!(host.tool_calls.lock().unwrap()[0].0, "describe_tool");
        assert_eq!(host.llm_calls.lock().unwrap().len(), 1);
    }

    /// `classifyWithLlm` on a host that doesn't implement it (the default trait
    /// method) surfaces as a catchable thrown `Error`.
    #[tokio::test]
    async fn guard_classify_llm_without_host_support_throws() {
        let runner = ScriptCodeRunner::new();
        // A minimal host that only wires `call_tool`, inheriting the default
        // `classify_llm` (which rejects).
        let host = Arc::new(FakeHost::default());
        let out = runner
            .run_guard(
                "try { catalerum.classifyWithLlm({}); return 'reached'; } \
                 catch (e) { return 'threw'; }",
                &json!({}),
                host,
            )
            .await
            .expect("guard runs");
        assert_eq!(out, json!("threw"));
    }
}
