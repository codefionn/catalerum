use super::*;

fn lazy_store() -> Store {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://localhost/catalerum_test")
        .expect("lazy pool");
    Store::new(pool)
}
use catalerum_storage::LocalFsBackend;

struct RecordingTerminalTool {
    name: &'static str,
    seen: Arc<Mutex<Option<Json>>>,
}

#[async_trait]
impl Tool for RecordingTerminalTool {
    fn name(&self) -> &str {
        self.name
    }
    /// Test tool: open.
    fn required_capability(&self) -> Option<catalerum_core::capability::Capability> {
        None
    }

    fn description(&self) -> &str {
        "test terminal tool"
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string" },
                "data": { "type": "string" }
            },
            "required": ["session_id"]
        })
    }

    async fn invoke(&self, args: Json, _ctx: &ToolContext) -> Result<Json> {
        *self.seen.lock().unwrap_or_else(|error| error.into_inner()) = Some(args.clone());
        Ok(args)
    }
}

fn recording_terminal_tool(name: &'static str) -> (Arc<dyn Tool>, Arc<Mutex<Option<Json>>>) {
    let seen = Arc::new(Mutex::new(None));
    (
        Arc::new(RecordingTerminalTool {
            name,
            seen: seen.clone(),
        }),
        seen,
    )
}

#[test]
fn terminal_command_probe_uses_completed_final_block_and_whitelist_order() {
    // PTYs echo the probe source first (including both marker literals). The
    // parser must ignore that and use only the final command-output block.
    let output = format!(
        "printf '{COMMAND_PROBE_START}'; echo python java '{COMMAND_PROBE_END}'\r\n\
         {COMMAND_PROBE_START}\r\nsqlite3\r\npython3\r\nnot-a-candidate\r\n\
         {COMMAND_PROBE_END}\r\n$ "
    );
    assert_eq!(
        parse_available_commands(&output),
        vec!["python3", "sqlite3"]
    );
    assert_eq!(TERMINAL_COMMAND_CANDIDATES.len(), 16);
    assert!(parse_available_commands("no completed marker").is_empty());
}

#[test]
fn terminal_subagent_network_policy_requires_explicit_isolation() {
    assert!(container_network_is_isolated("none"));
    assert!(!container_network_is_isolated(" ISOLATED "));
    assert!(!container_network_is_isolated("bridge"));
    assert!(sandbox_network_is_isolated("none"));
    assert!(sandbox_network_is_isolated(" ISOLATED "));
    assert!(!sandbox_network_is_isolated(""));
    assert!(!sandbox_network_is_isolated("egress"));
}

#[tokio::test]
async fn terminal_subagent_accepts_a_named_profile() {
    let store = lazy_store();
    let manager = Arc::new(TerminalManager::new(
        HashMap::new(),
        store.clone(),
        None,
        &ExecConfig::default(),
        None,
        "test-pod".into(),
    ));
    let tool = TerminalSubagentTool {
        manager,
        store,
        client: OpenRouterClient::new("http://localhost:9", ""),
        default_model: "default".into(),
        subagent_runs: SubagentRunManager::default(),
        run_cancel: None,
    };
    let schema = tool.parameters_schema();
    assert_eq!(schema["properties"]["profile"]["type"], "string");
    assert!(!schema["required"]
        .as_array()
        .expect("required array")
        .iter()
        .any(|name| name == "profile"));
}

#[tokio::test]
async fn terminal_subagent_registry_is_exact_and_pins_the_session() {
    let mut parent = ToolRegistry::new();
    let mut write_seen = None;
    for &name in TERMINAL_SUBAGENT_TOOLS {
        let (tool, seen) = recording_terminal_tool(name);
        if name == "terminal_write" {
            write_seen = Some(seen);
        }
        parent.register(tool);
    }
    parent.register(recording_terminal_tool("open_terminal").0);
    parent.register(recording_terminal_tool("store_object").0);
    parent.register(recording_terminal_tool("unrelated_parent_tool").0);
    let upstream = recording_terminal_tool(TERMINAL_UPSTREAM_NAME).0;
    let finish = recording_terminal_tool(TERMINAL_FINISH_NAME).0;
    let session_id = TerminalSessionId::new();

    let restricted =
        restricted_terminal_subagent_registry(&parent, session_id, upstream, finish);
    let mut names = restricted.names().map(str::to_string).collect::<Vec<_>>();
    names.sort();
    let mut expected = TERMINAL_SUBAGENT_TOOLS
        .iter()
        .map(|name| (*name).to_string())
        .chain([
            TERMINAL_UPSTREAM_NAME.to_string(),
            TERMINAL_FINISH_NAME.to_string(),
        ])
        .collect::<Vec<_>>();
    expected.sort();
    assert_eq!(names, expected);
    assert!(!restricted.contains("open_terminal"));
    assert!(!restricted.contains("store_object"));
    assert!(!restricted.contains("unrelated_parent_tool"));

    let write = restricted
        .get("terminal_write")
        .expect("terminal_write wrapper");
    assert!(write
        .parameters_schema()
        .pointer("/properties/session_id")
        .is_none());
    restricted
        .dispatch(
            "terminal_write",
            json!({ "session_id": TerminalSessionId::new(), "data": "cargo test\n" }),
            &crate::test_support::full_authority(),
        )
        .await
        .expect("pinned terminal dispatch");
    let seen = write_seen
        .expect("write recorder")
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
        .expect("recorded write args");
    assert_eq!(seen["session_id"], session_id.to_string());
    assert_eq!(seen["data"], "cargo test\n");
}

#[tokio::test]
async fn finish_work_records_one_evidence_backed_verdict() {
    let slot = Arc::new(Mutex::new(None));
    let tool = FinishTerminalWorkTool {
        verdict: slot.clone(),
    };
    let args = json!({
        "goal_met": true,
        "analysis": "Acceptance criteria pass.",
        "evidence": ["cargo test: green", "diff inspected"],
        "summary": "Implemented the scoped change."
    });
    let result = tool
        .invoke(args.clone(), &crate::test_support::full_authority())
        .await
        .expect("first verdict");
    assert_eq!(result["goal"]["met"], true);
    assert_eq!(
        slot.lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .map(|verdict| verdict.evidence.len()),
        Some(2)
    );
    assert!(tool.invoke(args, &crate::test_support::full_authority()).await.is_err());
}

#[test]
fn upstream_push_is_reported_only_after_a_met_goal_and_successful_finalizer() {
    assert!(finalizer_reported_push(
        true,
        &json!({ "pushed": true }),
        None
    ));
    assert!(!finalizer_reported_push(
        false,
        &json!({ "pushed": true }),
        None
    ));
    assert!(!finalizer_reported_push(
        true,
        &json!({ "pushed": false }),
        None
    ));
    assert!(!finalizer_reported_push(
        true,
        &json!({ "pushed": true }),
        Some("push failed")
    ));
}

#[tokio::test]
async fn terminal_upstream_dispatches_only_parent_selected_tools() {
    let mut parent = ToolRegistry::new();
    let (update_pr, seen) = recording_terminal_tool("update_pr");
    parent.register(update_pr);
    parent.register(recording_terminal_tool("update_other_pr").0);
    let upstream = TerminalUpstreamTool {
        description: "PR boundary".into(),
        allowed_tools: vec!["update_pr".into()],
        parent_registry: parent,
        parent_ctx: crate::test_support::full_authority(),
        cancel: CancellationToken::new(),
    };
    let result = upstream
        .invoke(
            json!({
                "tool": "update_pr",
                "arguments": { "pull_request_id": "pr-42", "body": "status" }
            }),
            &crate::test_support::full_authority(),
        )
        .await
        .expect("selected parent tool");
    assert_eq!(result["pull_request_id"], "pr-42");
    assert_eq!(
        seen.lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .and_then(|args| args.get("pull_request_id")),
        Some(&json!("pr-42"))
    );
    let denied = upstream
        .invoke(
            json!({ "tool": "update_other_pr", "arguments": {} }),
            &crate::test_support::full_authority(),
        )
        .await
        .expect_err("tool outside allow-list");
    assert!(denied.to_string().contains("upstream allow-list"));
}

fn db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

/// Multi-pod HA attach guard (SOUL §16 M7): driving a session with no live
/// handle on this pod yields a *precise* error — "owned by another pod" when a
/// still-active row belongs to a different pod (route there / sessionAffinity),
/// "no longer live on this pod" for one this pod owned but whose process ended,
/// and "unknown or closed" for a genuinely absent id. Gated on live Postgres
/// (it consults the durable row); no backend needed (the guard fires first).
#[tokio::test]
async fn attach_to_foreign_pod_session_errors_clearly() {
    let Some(url) = db_url() else {
        eprintln!("skipping attach_to_foreign_pod_session_errors_clearly: set CATALERUM_TEST_DATABASE_URL");
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("term-pod", &format!("term-pod-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");

    let cfg = ExecConfig {
        enabled: true,
        backend: "local".to_string(),
        ..Default::default()
    };
    // This pod is "pod-A"; no backends — the guard errors before any driver.
    let mgr = TerminalManager::new(
        HashMap::new(),
        store.clone(),
        None,
        &cfg,
        None,
        "pod-A".to_string(),
    );

    // A row a *different* pod owns (never in this manager's live map).
    let mk = |pod: Option<&str>| NewTerminalSession {
        backend: ExecutorKind::Local,
        host_dir: Some("/tmp/pod".into()),
        sync_prefix: None,
        pod_id: pod.map(str::to_string),
    };
    let foreign = store
        .terminal_sessions()
        .create(ws.id, &mk(Some("pod-B")))
        .await
        .expect("foreign row");
    let err = mgr
        .write(ws.id, foreign.id, b"x".to_vec())
        .await
        .expect_err("driving a foreign-pod session must error");
    let msg = err.to_string();
    assert!(
        msg.contains("owned by another pod") && msg.contains("pod-B"),
        "foreign-pod error should name the owner + advise routing, got: {msg}"
    );

    // A row this pod owns but that isn't live (process ended) → distinct message.
    let mine = store
        .terminal_sessions()
        .create(ws.id, &mk(Some("pod-A")))
        .await
        .expect("own row");
    let err = mgr.read(ws.id, mine.id, 0).await.expect_err("not live");
    assert!(
        err.to_string().contains("no longer live on this pod"),
        "own-but-dead session should say so, got: {err}"
    );

    // A genuinely unknown id → the generic message.
    let err = mgr
        .write(ws.id, TerminalSessionId::new(), b"x".to_vec())
        .await
        .expect_err("unknown id");
    assert!(
        err.to_string()
            .contains("unknown or closed terminal session"),
        "got: {err}"
    );
}

/// Foreign-pod file ops (SOUL §16 M7): a `session_host_dir`-backed op
/// (persist / read_file / edit_file) on a session with no live handle on this
/// pod must NOT blindly use the row's `host_dir` (a path on the *owning* pod) —
/// that died with a confusing raw I/O error. It now gives the same precise
/// verdict the attach guard does: "owned by another pod" (route there) for a
/// foreign row, "no longer live on this pod" for one this pod owned, and the
/// generic "unknown terminal session" for an absent id. Gated on live Postgres
/// (it consults the durable row); no backend/storage needed — the guard fires
/// before any file (or storage) touch.
#[tokio::test]
async fn foreign_pod_file_ops_error_clearly() {
    let Some(url) = db_url() else {
        eprintln!(
            "skipping foreign_pod_file_ops_error_clearly: set CATALERUM_TEST_DATABASE_URL"
        );
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("term-fpod", &format!("term-fpod-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");

    let cfg = ExecConfig {
        enabled: true,
        backend: "local".to_string(),
        ..Default::default()
    };
    // This pod is "pod-A"; no backends/storage — the host-dir guard errors
    // before any file (or storage) is touched.
    let mgr = TerminalManager::new(
        HashMap::new(),
        store.clone(),
        None,
        &cfg,
        None,
        "pod-A".to_string(),
    );

    // A row carries a `host_dir` naming the *owning* pod's fs — the old code
    // would try to read this path here and fail with a raw I/O error.
    let mk = |pod: Option<&str>| NewTerminalSession {
        backend: ExecutorKind::Local,
        host_dir: Some("/tmp/pod-owned-dir".into()),
        sync_prefix: None,
        pod_id: pod.map(str::to_string),
    };

    // A row a *different* pod owns → the foreign-pod verdict, not an I/O error.
    let foreign = store
        .terminal_sessions()
        .create(ws.id, &mk(Some("pod-B")))
        .await
        .expect("foreign row");
    // `read_file` returns a non-`Debug` `ReadFile`, so unwrap the error via
    // `.err()` rather than `.expect_err()` (which would bound the Ok type).
    let err = mgr
        .read_file(ws.id, foreign.id, "notes.txt", None, None)
        .await
        .err()
        .expect("reading a foreign-pod session's file must error precisely");
    let msg = err.to_string();
    assert!(
        msg.contains("owned by another pod")
            && msg.contains("pod-B")
            && msg.contains("session affinity"),
        "foreign-pod file op should name the owner + advise routing, got: {msg}"
    );
    // The edit path is `session_host_dir`-backed too → same precise verdict.
    let err = mgr
        .edit_file(ws.id, foreign.id, "notes.txt", "a", "b", false)
        .await
        .expect_err("editing a foreign-pod session's file must error precisely");
    assert!(
        err.to_string().contains("owned by another pod"),
        "got: {err}"
    );

    // A row this pod owns but with no live handle (process ended) → the distinct
    // "no longer live on this pod" message rather than a raw I/O error.
    let mine = store
        .terminal_sessions()
        .create(ws.id, &mk(Some("pod-A")))
        .await
        .expect("own row");
    let err = mgr
        .read_file(ws.id, mine.id, "notes.txt", None, None)
        .await
        .err()
        .expect("own-but-dead session file op must error");
    assert!(
        err.to_string().contains("no longer live on this pod"),
        "own-but-dead session should say so, got: {err}"
    );

    // A genuinely unknown id (row absent) → the generic message.
    let err = mgr
        .read_file(ws.id, TerminalSessionId::new(), "notes.txt", None, None)
        .await
        .err()
        .expect("unknown id");
    assert!(
        err.to_string().contains("unknown terminal session"),
        "got: {err}"
    );
}

#[test]
fn resolve_in_dir_joins_relative_and_rejects_escapes() {
    let base = Path::new("/work/abc");
    // A normal relative path joins under the workdir.
    assert_eq!(
        resolve_in_dir(base, "src/main.rs").unwrap(),
        base.join("src/main.rs")
    );
    // Interior `.` normalizes away (matches storage's key contract).
    assert_eq!(resolve_in_dir(base, "a/./b").unwrap(), base.join("a/b"));
    // Empty, absolute, and any `.`/`..` traversal are rejected — a file tool
    // can never escape the session's working directory (§18).
    for bad in ["", "   ", "/etc/passwd", "..", "../x", "a/../../b", "./a"] {
        assert!(
            resolve_in_dir(base, bad).is_err(),
            "{bad:?} should be rejected"
        );
    }
}

#[test]
fn in_container_path_joins_relative_and_rejects_escapes() {
    // Joins under the session's in-container workdir (trailing `/` tolerated).
    assert_eq!(
        in_container_path("/work/.ephemeral/abc", "klarna.pdf").unwrap(),
        "/work/.ephemeral/abc/klarna.pdf"
    );
    assert_eq!(
        in_container_path("/work/x/", "a/b.csv").unwrap(),
        "/work/x/a/b.csv"
    );
    // Same containment contract as `resolve_in_dir`, plus no newlines (the
    // path travels as one shell positional).
    for bad in ["", "  ", "/etc/passwd", "..", "../x", "a/../../b", "a\nb"] {
        assert!(
            in_container_path("/work/x", bad).is_err(),
            "{bad:?} should be rejected"
        );
    }
}

#[test]
fn cap_text_bounds_on_a_char_boundary() {
    let (out, trunc) = cap_text("hello", 100);
    assert_eq!(out, "hello");
    assert!(!trunc);
    // Over the cap → truncated, never split mid-codepoint (`é` is 2 bytes).
    let big = "é".repeat(100); // 200 bytes
    let (out, trunc) = cap_text(&big, 51);
    assert!(trunc);
    assert!(out.len() <= 51);
    assert!(big.starts_with(&out));
}

/// End-to-end: open an ephemeral local terminal, run a command, read its
/// output, persist the working dir to object storage, and close. Gated on a
/// live Postgres (+ a real PTY); skips offline.
#[tokio::test]
async fn manager_open_write_read_persist_close() {
    let Some(url) = db_url() else {
        eprintln!(
            "skipping manager_open_write_read_persist_close: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
        );
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("term-mgr", &format!("term-mgr-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");

    let local: Arc<dyn Executor> = Arc::new(catalerum_exec::LocalExecutor::new());
    let mut backends = HashMap::new();
    backends.insert(ExecutorKind::Local, local);

    let store_dir = tempfile::tempdir().unwrap();
    let storage = StorageHandle {
        backend: Arc::new(LocalFsBackend::new(store_dir.path().to_path_buf())),
        store: "default".to_string(),
        connection: "local-storage".to_string(),
        bucket: "default".to_string(),
        namespaced: true,
    };
    let eph = tempfile::tempdir().unwrap();
    let cfg = ExecConfig {
        enabled: true,
        backend: "local".to_string(),
        // Pin a deterministic shell — the dev user's $SHELL may be an
        // interactive zsh/p10k with a first-run wizard.
        shell: "/bin/sh".to_string(),
        ephemeral_root: eph.path().display().to_string(),
        ..Default::default()
    };
    let mgr = TerminalManager::new(
        backends,
        store.clone(),
        Some(storage),
        &cfg,
        None,
        "test-pod".to_string(),
    );

    let session = mgr.open(ws.id, None).await.expect("open");

    // The marker uses arithmetic expansion so it only appears in the command
    // *output* (`MARK42`), never in the PTY's echo of the typed input
    // (`echo MARK$((6*7))`) — otherwise we'd race ahead of execution.
    mgr.write(
        ws.id,
        session.id,
        b"printf alpha > out.txt; echo MARK$((6*7))\n".to_vec(),
    )
    .await
    .expect("write");

    let mut got = String::new();
    for _ in 0..150 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let bytes = mgr.read(ws.id, session.id, 0).await.expect("read");
        got.push_str(&String::from_utf8_lossy(&bytes));
        if got.contains("MARK42") {
            break;
        }
    }
    assert!(
        got.contains("MARK42"),
        "expected command output, got {got:?}"
    );
    assert_eq!(mgr.list(ws.id).await.expect("list").len(), 1);

    let keys = mgr
        .persist(ws.id, session.id, "snap", None)
        .await
        .expect("persist");
    assert!(keys.contains(&"snap/out.txt".to_string()), "keys: {keys:?}");
    let physical = store_dir
        .path()
        .join(ws.id.to_string())
        .join("snap")
        .join("out.txt");
    assert_eq!(
        tokio::fs::read(&physical).await.expect("persisted file"),
        b"alpha"
    );

    mgr.close(ws.id, session.id).await.expect("close");
    assert!(mgr.list(ws.id).await.expect("list2").is_empty());
    assert!(
        mgr.write(ws.id, session.id, b"x".to_vec()).await.is_err(),
        "writing a closed session must error"
    );
}

/// `store_object` (workdir → files store) is the exact inverse of
/// `stage_object` (files store → workdir): a file written into a session's
/// working directory streams out to the backend at its namespaced key, and
/// staging that key back into the session reproduces the original bytes. Gated
/// on a live Postgres (needs a real session row + host dir).
#[tokio::test]
async fn manager_store_object_round_trips_with_stage_object() {
    let Some(url) = db_url() else {
        eprintln!(
            "skipping manager_store_object_round_trips_with_stage_object: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
        );
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create(
            "term-store",
            &format!("term-store-{}", uuid::Uuid::new_v4()),
        )
        .await
        .expect("ws");

    let local: Arc<dyn Executor> = Arc::new(catalerum_exec::LocalExecutor::new());
    let mut backends = HashMap::new();
    backends.insert(ExecutorKind::Local, local);

    let store_dir = tempfile::tempdir().unwrap();
    let storage = StorageHandle {
        backend: Arc::new(LocalFsBackend::new(store_dir.path().to_path_buf())),
        store: "default".to_string(),
        connection: "local-storage".to_string(),
        bucket: "default".to_string(),
        namespaced: true,
    };
    let eph = tempfile::tempdir().unwrap();
    let cfg = ExecConfig {
        enabled: true,
        backend: "local".to_string(),
        shell: "/bin/sh".to_string(),
        ephemeral_root: eph.path().display().to_string(),
        ..Default::default()
    };
    let mgr = TerminalManager::new(
        backends,
        store.clone(),
        Some(storage),
        &cfg,
        None,
        "test-pod".to_string(),
    );
    let session = mgr.open(ws.id, None).await.expect("open");

    // A file the "shell" produced in the workdir.
    mgr.write_file(ws.id, session.id, "out/report.txt", "hello world")
        .await
        .expect("write_file");

    // store_object streams it out to the backend at the workspace-namespaced key.
    let backend = LocalFsBackend::new(store_dir.path().to_path_buf());
    let physical = workspace_object_key(ws.id, "artifacts/report.txt");
    let wrote = mgr
        .store_object(ws.id, session.id, &backend, &physical, "out/report.txt")
        .await
        .expect("store_object");
    assert_eq!(wrote, "hello world".len() as u64);
    let on_disk = store_dir
        .path()
        .join(ws.id.to_string())
        .join("artifacts/report.txt");
    assert_eq!(
        tokio::fs::read(&on_disk).await.expect("stored blob"),
        b"hello world"
    );

    // Staging that same key back into the session reproduces the bytes — the two
    // ops are inverses.
    let staged = mgr
        .stage_object(ws.id, session.id, &backend, &physical, "back/report.txt")
        .await
        .expect("stage_object");
    assert_eq!(staged, "hello world".len() as u64);
    let read = mgr
        .read_file(ws.id, session.id, "back/report.txt", None, None)
        .await
        .expect("read_file");
    assert_eq!(read.content, "hello world");

    // A directory source is rejected (not a file).
    assert!(
        mgr.store_object(ws.id, session.id, &backend, &physical, "out")
            .await
            .is_err(),
        "storing a directory must error"
    );

    mgr.close(ws.id, session.id).await.expect("close");
}

/// The workdir file tools end-to-end: open an ephemeral local terminal, then
/// create / read / edit files in its working directory and assert both the
/// tool results and the on-disk bytes. No object storage needed (these are
/// `exec:run` workdir ops). Gated on a live Postgres (+ a real PTY).
#[tokio::test]
async fn file_tools_create_read_edit() {
    let Some(url) = db_url() else {
        eprintln!(
            "skipping file_tools_create_read_edit: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
        );
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create(
            "term-files",
            &format!("term-files-{}", uuid::Uuid::new_v4()),
        )
        .await
        .expect("ws");

    let local: Arc<dyn Executor> = Arc::new(catalerum_exec::LocalExecutor::new());
    let mut backends = HashMap::new();
    backends.insert(ExecutorKind::Local, local);
    let eph = tempfile::tempdir().unwrap();
    let cfg = ExecConfig {
        enabled: true,
        backend: "local".to_string(),
        shell: "/bin/sh".to_string(),
        ephemeral_root: eph.path().display().to_string(),
        ..Default::default()
    };
    // No storage handle — the file tools don't touch object storage.
    let mgr = TerminalManager::new(
        backends,
        store.clone(),
        None,
        &cfg,
        None,
        "test-pod".to_string(),
    );
    let session = mgr.open(ws.id, None).await.expect("open");
    let host = PathBuf::from(
        session
            .host_dir
            .clone()
            .expect("local backend has a host dir"),
    );

    // create_file writes through (and makes parent dirs); the bytes land on disk.
    let body = "alpha\nbeta\ngamma\n";
    let (n, overwrote) = mgr
        .write_file(ws.id, session.id, "src/app.txt", body)
        .await
        .expect("create");
    assert_eq!(n, body.len() as u64);
    assert!(!overwrote, "a fresh file is not an overwrite");
    assert_eq!(
        tokio::fs::read_to_string(host.join("src/app.txt"))
            .await
            .unwrap(),
        body
    );

    // read_file returns the whole file, then a single-line window.
    let whole = mgr
        .read_file(ws.id, session.id, "src/app.txt", None, None)
        .await
        .expect("read");
    assert_eq!(whole.content, body);
    assert_eq!(whole.total_lines, 3);
    assert!(!whole.truncated);
    let line2 = mgr
        .read_file(ws.id, session.id, "src/app.txt", Some(2), Some(1))
        .await
        .expect("read window");
    assert_eq!(line2.content, "beta");

    // edit_file: a unique replacement succeeds and persists.
    let made = mgr
        .edit_file(ws.id, session.id, "src/app.txt", "beta", "BETA", false)
        .await
        .expect("edit unique");
    assert_eq!(made, 1);
    assert_eq!(
        tokio::fs::read_to_string(host.join("src/app.txt"))
            .await
            .unwrap(),
        "alpha\nBETA\ngamma\n"
    );

    // A non-unique edit is refused without replace_all, accepted with it.
    mgr.write_file(ws.id, session.id, "dup.txt", "x x x")
        .await
        .expect("dup");
    assert!(
        mgr.edit_file(ws.id, session.id, "dup.txt", "x", "y", false)
            .await
            .is_err(),
        "ambiguous edit must error without replace_all"
    );
    let made = mgr
        .edit_file(ws.id, session.id, "dup.txt", "x", "y", true)
        .await
        .expect("edit all");
    assert_eq!(made, 3);
    assert_eq!(
        tokio::fs::read_to_string(host.join("dup.txt"))
            .await
            .unwrap(),
        "y y y"
    );

    // Path traversal can't escape the workdir; a missing file is a clean error.
    assert!(
        mgr.read_file(ws.id, session.id, "../escape", None, None)
            .await
            .is_err(),
        "traversal must be rejected"
    );
    assert!(
        mgr.read_file(ws.id, session.id, "nope.txt", None, None)
            .await
            .is_err(),
        "a missing file errors"
    );

    mgr.close(ws.id, session.id).await.expect("close");
}

fn podman_available() -> bool {
    std::process::Command::new("podman")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The workdir file tools on a **sandboxed** (container-backed) terminal
/// (SOUL §20): create / read / window / edit ride the workspace sandbox's
/// copy channel instead of a host dir — the session has no `host_dir`, the
/// bytes live in the container, and every op stages through a temp file.
/// Gated on live Postgres + podman; skips offline.
#[tokio::test]
async fn sandboxed_file_tools_ride_the_copy_channel() {
    let Some(url) = db_url() else {
        eprintln!(
            "skipping sandboxed_file_tools_ride_the_copy_channel: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
        );
        return;
    };
    if !podman_available() {
        eprintln!("skipping sandboxed_file_tools_ride_the_copy_channel: podman not available");
        return;
    }
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("term-sbx", &format!("term-sbx-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");

    let spec = catalerum_exec::SandboxSpec {
        image: Some("docker.io/library/busybox:latest".to_string()),
        ..Default::default()
    };
    let pod_sandbox = catalerum_exec::PodmanSandbox::new("podman", spec.clone());
    let sandbox_mgr = Arc::new(crate::sandbox::WorkspaceSandboxManager::new(
        Arc::new(pod_sandbox.clone()),
        store.clone(),
        ExecutorKind::Container,
        spec,
        std::time::Duration::from_secs(600),
        "test-pod".to_string(),
    ));
    let eph = tempfile::tempdir().unwrap();
    let cfg = ExecConfig {
        enabled: true,
        backend: "container".to_string(),
        shell: "/bin/sh".to_string(),
        ephemeral_root: eph.path().display().to_string(),
        ..Default::default()
    };
    // No per-call backends: every container terminal runs inside the
    // workspace sandbox.
    let mgr = TerminalManager::new(
        HashMap::new(),
        store.clone(),
        None,
        &cfg,
        Some(sandbox_mgr),
        "test-pod".to_string(),
    );
    let session = match mgr.open(ws.id, None).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("skipping sandboxed_file_tools: open failed: {e}");
            return;
        }
    };
    assert!(
        session.host_dir.is_none(),
        "a sandboxed session keeps its files in the container"
    );

    // create_file rides the copy channel (in-container parent dirs made).
    let body = "alpha\nbeta\ngamma\n";
    let (n, overwrote) = mgr
        .write_file(ws.id, session.id, "src/app.txt", body)
        .await
        .expect("create");
    assert_eq!(n, body.len() as u64);
    assert!(!overwrote, "a fresh file is not an overwrite");
    // A second write is detected as an overwrite by the existence probe.
    let (_, overwrote) = mgr
        .write_file(ws.id, session.id, "src/app.txt", body)
        .await
        .expect("overwrite");
    assert!(overwrote);

    // read_file stages the container file out and round-trips the bytes.
    let whole = mgr
        .read_file(ws.id, session.id, "src/app.txt", None, None)
        .await
        .expect("read");
    assert_eq!(whole.content, body);
    assert_eq!(whole.total_lines, 3);
    assert!(!whole.truncated);
    let line2 = mgr
        .read_file(ws.id, session.id, "src/app.txt", Some(2), Some(1))
        .await
        .expect("read window");
    assert_eq!(line2.content, "beta");

    // edit_file stages out, replaces, and copies back — the follow-up read
    // (freshly staged from the container) proves the write-back landed.
    let made = mgr
        .edit_file(ws.id, session.id, "src/app.txt", "beta", "BETA", false)
        .await
        .expect("edit unique");
    assert_eq!(made, 1);
    let after = mgr
        .read_file(ws.id, session.id, "src/app.txt", None, None)
        .await
        .expect("read after edit");
    assert_eq!(after.content, "alpha\nBETA\ngamma\n");

    // The ambiguity contract holds on the sandbox path too.
    mgr.write_file(ws.id, session.id, "dup.txt", "x x x")
        .await
        .expect("dup");
    assert!(
        mgr.edit_file(ws.id, session.id, "dup.txt", "x", "y", false)
            .await
            .is_err(),
        "ambiguous edit must error without replace_all"
    );
    assert_eq!(
        mgr.edit_file(ws.id, session.id, "dup.txt", "x", "y", true)
            .await
            .expect("edit all"),
        3
    );

    // Missing files and traversal are clean errors.
    assert!(mgr
        .read_file(ws.id, session.id, "nope.txt", None, None)
        .await
        .is_err());
    assert!(mgr
        .read_file(ws.id, session.id, "../escape", None, None)
        .await
        .is_err());

    mgr.close(ws.id, session.id).await.expect("close");
    // Tear the workspace container down (its named volume is kept by design).
    use catalerum_exec::WorkspaceSandbox as _;
    pod_sandbox.destroy(ws.id).await.expect("destroy");
}

/// The self-exit reaper end-to-end (SOUL §20): an ephemeral local session
/// whose shell runs `exit` is reaped without an explicit `close` — its live
/// handle dropped, the DB row marked closed, and the temp dir removed. Each
/// manager owns a fresh executor + workspace, so this needs no serialization.
/// Gated on a live Postgres (+ a real PTY); skips offline.
#[tokio::test]
async fn reap_closes_a_self_exited_session() {
    let Some(url) = db_url() else {
        eprintln!(
            "skipping reap_closes_a_self_exited_session: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
        );
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("term-reap", &format!("term-reap-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");

    let local: Arc<dyn Executor> = Arc::new(catalerum_exec::LocalExecutor::new());
    let mut backends = HashMap::new();
    backends.insert(ExecutorKind::Local, local);
    let eph = tempfile::tempdir().unwrap();
    let cfg = ExecConfig {
        enabled: true,
        backend: "local".to_string(),
        shell: "/bin/sh".to_string(),
        ephemeral_root: eph.path().display().to_string(),
        ..Default::default()
    };
    let mgr = TerminalManager::new(
        backends,
        store.clone(),
        None,
        &cfg,
        None,
        "test-pod".to_string(),
    );
    let session = mgr.open(ws.id, None).await.expect("open");
    let host = session
        .host_dir
        .clone()
        .expect("local backend has a host dir");
    assert_eq!(mgr.list(ws.id).await.expect("list").len(), 1);

    // The shell exits on its own — no explicit close_terminal.
    mgr.write(ws.id, session.id, b"exit\n".to_vec())
        .await
        .expect("write exit");

    // Poll the reaper until it observes the exit (the PTY exit is async).
    let mut reaped = 0;
    for _ in 0..200 {
        reaped = mgr.reap().await.expect("reap");
        if reaped > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(reaped, 1, "the self-exited session is reaped");

    // The DB row is now closed (list empty) and the live handle is gone.
    assert!(mgr.list(ws.id).await.expect("list2").is_empty());
    assert!(
        mgr.write(ws.id, session.id, b"x".to_vec()).await.is_err(),
        "writing a reaped session must error"
    );
    // The ephemeral temp dir was cleaned up, and a second reap finds nothing.
    assert!(!Path::new(&host).exists(), "ephemeral dir removed on reap");
    assert_eq!(mgr.reap().await.expect("reap2"), 0, "reap is idempotent");
}

#[test]
fn text_reads_reject_invalid_utf8_and_binary_control_bytes() {
    assert_eq!(
        text_file_content(b"hello\nworld".to_vec(), "ok.txt", false).unwrap(),
        "hello\nworld"
    );
    assert!(text_file_content(vec![0xff, 0xfe], "binary.dat", false)
        .unwrap_err()
        .to_string()
        .contains("binary"));
    assert!(
        text_file_content(b"hello\0world".to_vec(), "binary.dat", false)
            .unwrap_err()
            .to_string()
            .contains("control")
    );
    assert!(text_file_content(vec![1, 2, 3], "binary.dat", false)
        .unwrap_err()
        .to_string()
        .contains("control"));
    assert_eq!(
        text_file_content(vec![b'a', 0xe2, 0x82], "large.txt", true).unwrap(),
        "a"
    );
}

#[test]
fn read_file_description_and_media_detection_follow_model_modalities() {
    let text_only = read_file_description("Read text.", &["text".into()]);
    assert!(text_only.contains("Binary files are rejected"));
    assert!(text_only.contains("unavailable"));
    assert!(text_only.contains("llmleaf"));

    let multimodal = read_file_description("Read text.", &["text".into(), "image".into()]);
    assert!(multimodal.contains("image input"));
    assert!(multimodal.contains("llmleaf"));
    assert!(multimodal.contains("ingested natively"));

    assert_eq!(
        supported_media_path("frame.png", &["text".into(), "image".into()]),
        Some("image/png")
    );
    assert_eq!(
        supported_media_path("clip.mp4", &["text".into(), "image".into()]),
        None
    );
}
