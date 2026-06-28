use super::*;

#[tokio::test]
async fn existing_ui_tool_schemas_are_moonshot_compatible() {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://localhost/catalerum_test")
        .expect("lazy pool");
    let uis = Store::new(pool).ui_definitions();
    let allow = Arc::new(HashSet::new());
    let tools: Vec<Box<dyn Tool>> = vec![
        Box::new(CreateUiComponentsTool {
            uis: uis.clone(),
            allow: allow.clone(),
        }),
        Box::new(EditUiComponentsTool {
            uis: uis.clone(),
            allow: allow.clone(),
        }),
        Box::new(EditUiTool {
            uis: uis.clone(),
            allow,
        }),
        Box::new(ReadUiTool { uis }),
    ];

    for tool in tools {
        let parameters = tool.parameters_schema();
        assert_eq!(
            parameters["type"],
            "object",
            "{} keeps the provider-required root object type",
            tool.name()
        );
        assert!(
            parameters.get("anyOf").is_none(),
            "{} avoids a Moonshot-incompatible root union",
            tool.name()
        );
        assert!(
            parameters["properties"].get("id").is_some()
                && parameters["properties"].get("name").is_some(),
            "{} still advertises both supported App targets",
            tool.name()
        );
    }
}

#[tokio::test]
async fn ui_schema_discovery_and_focused_lookup_are_split() {
    assert!(is_ui_authoring_tool("present_ui"));
    assert!(is_ui_authoring_tool("create_ui_components"));
    assert!(is_ui_authoring_tool("edit_ui_components"));
    assert!(is_ui_authoring_tool("edit_ui"));
    assert!(!is_ui_authoring_tool("read_ui"));

    let context = ToolContext::default();
    let index = ExplainUiSchemaTool
        .invoke(json!({}), &context)
        .await
        .expect("schema index");
    assert!(index["components"]["containers"]
        .as_array()
        .is_some_and(|kinds| kinds.iter().any(|kind| kind == "stack")));
    assert!(index["components"]["containers"]
        .as_array()
        .is_some_and(|kinds| kinds.iter().any(|kind| kind == "constrained_box")));
    assert!(index["topics"]
        .as_array()
        .is_some_and(|topics| topics.iter().any(|topic| topic == "binding")));
    assert!(index["topics"]
        .as_array()
        .is_some_and(|topics| topics.iter().any(|topic| topic == "navigation")));
    assert!(
        index.get("props_by_kind").is_none(),
        "the discovery call stays compact"
    );

    let tool = GetUiSchemaTool;
    assert_eq!(tool.parameters_schema()["required"], json!(["components"]));
    let advertised_kinds = tool.parameters_schema()["properties"]["components"]["items"]["enum"]
        .as_array()
        .expect("component enum")
        .clone();
    assert!(advertised_kinds.iter().any(|kind| kind == "stack"));
    assert!(advertised_kinds.iter().any(|kind| kind == "aspect_ratio"));
    assert!(!advertised_kinds.iter().any(|kind| kind == "chip"));
    assert!(!advertised_kinds.iter().any(|kind| kind == "scroll_view"));

    let focused = tool
        .invoke(
            json!({
                "components": ["donut_chart", "text_input", "donut_chart"],
                "topics": ["binding"]
            }),
            &context,
        )
        .await
        .expect("focused schema");
    let components = focused["components"].as_object().expect("components");
    assert_eq!(components.len(), 2, "duplicate requests are collapsed");
    assert_eq!(components["donut_chart"]["category"], "charts");
    assert!(components["donut_chart"]["props"]
        .as_array()
        .is_some_and(|details| details.len() == 2));
    assert_eq!(components["text_input"]["category"], "inputs");
    let topics = focused["topics"].as_object().expect("topics");
    assert_eq!(topics.len(), 1);
    assert!(topics.contains_key("binding"));

    let layout = tool
        .invoke(
            json!({ "components": ["constrained_box", "aspect_ratio"] }),
            &context,
        )
        .await
        .expect("size wrapper schema");
    assert_eq!(
        layout["components"]["constrained_box"]["category"],
        "containers"
    );
    assert!(layout["components"]["constrained_box"]["props"]
        .as_array()
        .is_some_and(|details| details.iter().any(|detail| detail
            .as_str()
            .is_some_and(|text| text.contains("max_width")))));

    let app_patterns = tool
        .invoke(
            json!({
                "components": ["button"],
                "topics": ["navigation", "external_db"]
            }),
            &context,
        )
        .await
        .expect("App authoring patterns");
    assert!(app_patterns["topics"]["navigation"]
        .as_str()
        .is_some_and(|guide| guide.contains("separate VIEW") && guide.contains("selectedId")));
    assert!(app_patterns["topics"]["external_db"]
        .as_str()
        .is_some_and(|guide| guide.contains("db_result.rows") && guide.contains("root load")));

    assert!(tool
        .invoke(json!({ "components": [] }), &context)
        .await
        .is_err());
    assert!(tool
        .invoke(json!({ "components": ["not_a_component"] }), &context)
        .await
        .is_err());
}

/// Every argument spelling models produce for `create_calendar_connection`
/// folds into the route body's nested `config` shape (the strip-prone
/// `{"type":"object"}` param trap — see [`normalize_calendar_connection_args`]).
#[test]
fn normalize_calendar_connection_args_accepts_all_spellings() {
    // Flat top-level settings (the advertised schema) nest under `config`.
    let out = normalize_calendar_connection_args(json!({
        "kind": "webcal", "name": "Feiertage",
        "base_url": "https://example.org/feiertage.ics"
    }))
    .unwrap();
    assert_eq!(
        out["config"]["base_url"],
        json!("https://example.org/feiertage.ics")
    );
    assert!(
        out.get("base_url").is_none(),
        "flat key moved, not duplicated"
    );

    // The documented nested shape still passes through untouched.
    let out = normalize_calendar_connection_args(json!({
        "kind": "caldav", "name": "Work",
        "config": { "base_url": "https://dav.example/cal", "username": "u" }
    }))
    .unwrap();
    assert_eq!(out["config"]["base_url"], json!("https://dav.example/cal"));
    assert_eq!(out["config"]["username"], json!("u"));

    // A double-encoded (stringified JSON) config is parsed back.
    let out = normalize_calendar_connection_args(json!({
        "kind": "webcal", "name": "Feiertage",
        "config": "{\"base_url\": \"https://example.org/f.ics\"}"
    }))
    .unwrap();
    assert_eq!(
        out["config"]["base_url"],
        json!("https://example.org/f.ics")
    );

    // A bare-string config is filed under the kind's required key.
    let out = normalize_calendar_connection_args(json!({
        "kind": "webcal", "name": "Feiertage",
        "config": "https://example.org/f.ics"
    }))
    .unwrap();
    assert_eq!(
        out["config"]["base_url"],
        json!("https://example.org/f.ics")
    );
    let out = normalize_calendar_connection_args(json!({
        "kind": "local", "name": "Team", "config": "/srv/cal"
    }))
    .unwrap();
    assert_eq!(out["config"]["dir"], json!("/srv/cal"));

    // Nested config wins over a conflicting flat key; an empty-string config
    // degrades to {} so validation still names the missing field.
    let out = normalize_calendar_connection_args(json!({
        "kind": "webcal", "name": "F",
        "base_url": "https://flat.example/f.ics",
        "config": { "base_url": "https://nested.example/f.ics" }
    }))
    .unwrap();
    assert_eq!(
        out["config"]["base_url"],
        json!("https://nested.example/f.ics")
    );
    let out = normalize_calendar_connection_args(json!({
        "kind": "webcal", "name": "F", "config": ""
    }))
    .unwrap();
    assert_eq!(out["config"], json!({}));
}

#[test]
fn compile_automation_triggers_validates_linear_and_graph() {
    // Linear: a valid trigger/action pair returns the triggers unchanged.
    let t = compile_automation_triggers(
        None,
        vec![json!({ "kind": "schedule", "cron": "0 9 * * *" })],
        None,
        &[json!({ "kind": "summarize" })],
    )
    .unwrap();
    assert_eq!(t.len(), 1);
    assert_eq!(t[0]["kind"], json!("schedule"));

    // Linear: an invalid cron / empty actions is rejected.
    assert!(compile_automation_triggers(
        None,
        vec![json!({ "kind": "schedule", "cron": "nope" })],
        None,
        &[json!({ "kind": "summarize" })],
    )
    .is_err());

    // Graph: a webhook→note graph compiles its Trigger node into a dispatch
    // trigger (the linear `triggers`/`actions` args are ignored when a graph is
    // present).
    let spec = json!({ "graph": {
        "nodes": [
            { "id": "t", "kind": "trigger", "trigger": { "kind": "webhook", "path": "/h" } },
            { "id": "a", "kind": "action", "action": { "kind": "create_note", "title": "x" } }
        ],
        "edges": [ { "from": "t", "to": "a" } ]
    }});
    let t = compile_automation_triggers(Some(&spec), vec![], None, &[]).unwrap();
    assert_eq!(t.len(), 1);
    assert_eq!(t[0]["kind"], json!("webhook"));

    // Graph: a triggerless graph is rejected.
    let bad = json!({ "graph": { "nodes": [
        { "id": "a", "kind": "action", "action": { "kind": "summarize" } }
    ], "edges": [] }});
    assert!(compile_automation_triggers(Some(&bad), vec![], None, &[]).is_err());
}

#[test]
fn graph_warnings_flags_disconnected_nodes_and_is_empty_otherwise() {
    // A wired graph (and any linear/non-graph spec) yields no warnings.
    let wired = json!({ "graph": {
        "nodes": [
            { "id": "t", "kind": "trigger", "trigger": { "kind": "webhook", "path": "/h" } },
            { "id": "a", "kind": "action", "action": { "kind": "summarize" } }
        ],
        "edges": [ { "from": "t", "to": "a" } ]
    }});
    assert!(graph_warnings(Some(&wired)).is_empty());
    assert!(graph_warnings(None).is_empty(), "no spec → no warnings");
    assert!(
        graph_warnings(Some(&json!({ "note": "linear" }))).is_empty(),
        "non-graph spec → no warnings"
    );

    // A disconnected action node is flagged (it never runs).
    let islanded = json!({ "graph": {
        "nodes": [
            { "id": "t", "kind": "trigger", "trigger": { "kind": "webhook", "path": "/h" } },
            { "id": "a", "kind": "action", "action": { "kind": "summarize" } }
        ],
        "edges": []
    }});
    let w = graph_warnings(Some(&islanded));
    assert!(
        w.iter().any(|m| m.contains("node 'a' is not connected")),
        "{w:?}"
    );
}

#[test]
fn automation_response_attaches_warnings_without_dropping_fields() {
    use catalerum_core::{Automation, WorkspaceId};
    let automation = Automation {
        id: catalerum_core::AutomationId::new(),
        workspace_id: WorkspaceId::new(),
        name: "daily".into(),
        enabled: true,
        triggers: vec![json!({ "kind": "webhook", "path": "/h" })],
        condition: None,
        actions: vec![],
        spec: None,
        grant_id: None,
    };
    // Clean: the automation's own fields survive and `warnings` is an empty array.
    let clean = automation_response(automation.clone(), vec![]).unwrap();
    assert_eq!(clean["name"], json!("daily"));
    assert_eq!(clean["enabled"], json!(true));
    assert_eq!(clean["warnings"], json!([]));
    // With warnings: they ride alongside the automation payload.
    let warned = automation_response(automation, vec!["node 'a' is not connected".into()]).unwrap();
    assert_eq!(warned["name"], json!("daily"));
    assert_eq!(warned["warnings"][0], json!("node 'a' is not connected"));
}

#[tokio::test]
async fn test_automation_tool_dry_runs_without_persisting() {
    let tool = TestAutomationTool;
    let ctx = ToolContext::default();

    // A valid linear draft reports valid + its trigger kinds.
    let out = tool
        .invoke(
            json!({
                "triggers": [{ "kind": "schedule", "cron": "0 9 * * *" }],
                "actions": [{ "kind": "summarize" }]
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(out["valid"], json!(true));
    assert_eq!(out["kind"], json!("linear"));
    assert_eq!(out["trigger_kinds"], json!(["schedule"]));

    // An invalid draft reports valid:false with the error (no tool-call failure,
    // so the agent can iterate).
    let out = tool
        .invoke(
            json!({ "triggers": [{ "kind": "schedule", "cron": "bad" }], "actions": [{ "kind": "summarize" }] }),
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(out["valid"], json!(false));
    assert!(out["error"].as_str().is_some());

    // A graph draft reports the compiled triggers + node/edge counts.
    let out = tool
        .invoke(
            json!({ "spec": { "graph": {
                "nodes": [
                    { "id": "t", "kind": "trigger", "trigger": { "kind": "webhook", "path": "/h" } },
                    { "id": "a", "kind": "action", "action": { "kind": "create_note", "title": "x" } }
                ],
                "edges": [ { "from": "t", "to": "a" } ]
            }}}),
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(out["valid"], json!(true));
    assert_eq!(out["kind"], json!("graph"));
    assert_eq!(out["node_count"], json!(2));
    assert_eq!(out["compiled_triggers"][0]["kind"], json!("webhook"));
    // A wired graph reports an empty `warnings` array (the field is always present).
    assert_eq!(out["warnings"], json!([]));

    // A graph that VALIDATES but leaves a node disconnected still reports valid:true
    // (it saves), and surfaces the dead node in `warnings` so the author can fix it.
    let out = tool
        .invoke(
            json!({ "spec": { "graph": {
                "nodes": [
                    { "id": "t", "kind": "trigger", "trigger": { "kind": "webhook", "path": "/h" } },
                    { "id": "a", "kind": "action", "action": { "kind": "summarize" } }
                ],
                "edges": []
            }}}),
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(
        out["valid"],
        json!(true),
        "a disconnected node still validates"
    );
    let warnings = out["warnings"].as_array().expect("warnings array");
    assert!(
        warnings.iter().any(|m| m
            .as_str()
            .is_some_and(|s| s.contains("node 'a' is not connected"))),
        "{warnings:?}"
    );
}

#[tokio::test]
async fn node_type_catalog_tools_list_filter_and_get() {
    let ctx = ToolContext::default();

    // list (unfiltered) returns every node type with a compact shape.
    let out = ListAutomationNodeTypesTool
        .invoke(json!({}), &ctx)
        .await
        .unwrap();
    let all = out["node_types"].as_array().unwrap();
    assert_eq!(all.len(), catalerum_automation::catalog().len());
    assert!(all
        .iter()
        .all(|n| n.get("id").is_some() && n.get("summary").is_some()));

    // list filtered to triggers returns only triggers (11 of them).
    let out = ListAutomationNodeTypesTool
        .invoke(json!({ "node_kind": "trigger" }), &ctx)
        .await
        .unwrap();
    let triggers = out["node_types"].as_array().unwrap();
    assert_eq!(triggers.len(), 11);
    assert!(triggers.iter().all(|n| n["node_kind"] == json!("trigger")));

    // get by id returns the full doc (params + example), and an unknown id errors.
    let out = GetAutomationNodeTypeTool
        .invoke(json!({ "id": "trigger.schedule" }), &ctx)
        .await
        .unwrap();
    assert_eq!(out["id"], json!("trigger.schedule"));
    assert_eq!(out["example"]["trigger"]["kind"], json!("schedule"));
    assert!(out["params"].as_array().is_some_and(|p| !p.is_empty()));
    assert!(GetAutomationNodeTypeTool
        .invoke(json!({ "id": "nope" }), &ctx)
        .await
        .is_err());
}

#[test]
fn parse_mcp_server_validates_per_transport_and_parses_fields() {
    // stdio (default) requires a command.
    assert!(parse_mcp_server(&json!({ "name": "x" })).is_err());
    let s = parse_mcp_server(&json!({
        "name": "pw",
        "command": "npx",
        "args": ["@playwright/mcp", "--headless"]
    }))
    .unwrap();
    assert_eq!(s.transport, "stdio");
    assert_eq!(s.command, "npx");
    assert_eq!(
        s.args,
        vec!["@playwright/mcp".to_string(), "--headless".to_string()]
    );
    assert!(s.enabled, "enabled defaults true");

    // http requires a url; auth + env parse into the typed spec/map.
    assert!(parse_mcp_server(&json!({ "name": "h", "transport": "http" })).is_err());
    let h = parse_mcp_server(&json!({
        "name": "acme",
        "transport": "http",
        "url": "https://acme/mcp",
        "auth": { "kind": "bearer", "token": "sekret" },
        "env": { "A": "1" },
        "enabled": false
    }))
    .unwrap();
    assert_eq!(h.transport, "http");
    assert_eq!(h.url, "https://acme/mcp");
    assert_eq!(h.auth.kind, "bearer");
    assert_eq!(h.auth.token, "sekret");
    assert_eq!(h.env.get("A").map(String::as_str), Some("1"));
    assert!(!h.enabled);
}

#[test]
fn redact_mcp_server_never_echoes_secrets() {
    let def = McpServerDef {
        id: catalerum_core::McpServerId::new(),
        workspace_id: catalerum_core::WorkspaceId::new(),
        name: "acme".into(),
        transport: "http".into(),
        command: String::new(),
        args: vec![],
        env: BTreeMap::from([("TOKEN_ENV".to_string(), "leakme".to_string())]),
        url: "https://acme/mcp".into(),
        auth: McpAuthSpec {
            kind: "bearer".into(),
            token: "sekret".into(),
            ..Default::default()
        },
        enabled: true,
        tools: vec![],
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    let v = redact_mcp_server(&def, true);
    let dump = v.to_string();
    // Neither the auth secret nor env *values* appear; only keys + a flag.
    assert!(!dump.contains("sekret"), "auth secret leaked: {dump}");
    assert!(!dump.contains("leakme"), "env value leaked: {dump}");
    assert_eq!(v["auth_kind"], json!("bearer"));
    assert_eq!(v["auth_has_secret"], json!(true));
    assert_eq!(v["env_keys"], json!(["TOKEN_ENV"]));
    assert_eq!(v["connected"], json!(true));
}

#[test]
fn opt_tags_trims_dedups_and_drops_empty() {
    let args = json!({ "tags": ["  work ", "work", "", "  ", "ideas"] });
    assert_eq!(
        opt_tags(&args),
        vec!["work".to_string(), "ideas".to_string()]
    );
}

#[test]
fn opt_tags_absent_is_empty() {
    assert!(opt_tags(&json!({})).is_empty());
}

#[test]
fn cap_read_text_truncates_on_a_char_boundary() {
    // Short text passes through untouched.
    let (out, trunc) = cap_read_text("hello");
    assert_eq!(out, "hello");
    assert!(!trunc);
    // Exactly at the cap is not truncated.
    assert!(!cap_read_text(&"a".repeat(MAX_READ_TEXT_BYTES)).1);
    // Over the cap → truncated to a prefix within the byte budget, never
    // splitting a 2-byte char.
    let big = "é".repeat(MAX_READ_TEXT_BYTES); // 2 bytes each → 2× over
    let (out, trunc) = cap_read_text(&big);
    assert!(trunc);
    assert!(out.len() <= MAX_READ_TEXT_BYTES);
    assert!(big.starts_with(&out));
}

#[test]
fn match_snippet_centres_on_the_match_and_ellipsizes() {
    // Short content (≤ max) is returned whole, no ellipsis.
    assert_eq!(match_snippet("a short line", "short", 240), "a short line");

    // A match deep in long content → a centred, doubly-ellipsized window that
    // still contains the term and is bounded by `max` chars.
    let long = format!("{}NEEDLE{}", "a".repeat(100), "b".repeat(100));
    let s = match_snippet(&long, "needle", 40); // case-insensitive
    assert!(s.contains("NEEDLE"), "snippet keeps the match: {s}");
    assert!(
        s.starts_with('…') && s.ends_with('…'),
        "clipped both ends: {s}"
    );
    // Window is `max` chars + up to two ellipsis chars.
    assert!(
        s.chars().count() <= 40 + 2,
        "bounded length: {}",
        s.chars().count()
    );

    // A match near the start → no leading ellipsis (window pinned to the head).
    let head = format!("NEEDLE {}", "z".repeat(300));
    let s2 = match_snippet(&head, "needle", 40);
    assert!(
        !s2.starts_with('…'),
        "no leading ellipsis at the head: {s2}"
    );
    assert!(s2.ends_with('…') && s2.contains("NEEDLE"));

    // Char-safe on multi-byte content (no panic, bounded).
    let multi = "é".repeat(300);
    let s3 = match_snippet(&multi, "x", 40);
    assert!(s3.chars().count() <= 40 + 2);
}

#[test]
fn truncate_chars_caps_char_safely_and_flags_clipping() {
    // Under the cap → unchanged, not flagged.
    let (out, trunc) = truncate_chars("hello", 10);
    assert_eq!(out, "hello");
    assert!(!trunc);
    // Exactly at the cap → unchanged.
    assert!(!truncate_chars(&"a".repeat(10), 10).1);
    // Over the cap → clipped to `max` chars, flagged.
    let (out, trunc) = truncate_chars(&"a".repeat(50), 10);
    assert_eq!(out.chars().count(), 10);
    assert!(trunc);
    // Multi-byte: caps by CHARS (not bytes) and never splits a boundary.
    let (out, trunc) = truncate_chars(&"é".repeat(50), 10);
    assert_eq!(out.chars().count(), 10);
    assert_eq!(out, "é".repeat(10));
    assert!(trunc);
}

#[tokio::test]
async fn notify_tool_routes_by_channel_name_and_is_capability_gated() {
    use catalerum_channels::DiscordWebhookChannel;
    use catalerum_core::model::Role;
    use std::collections::HashMap;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // Two named channels at two distinct mock servers.
    let default_srv = MockServer::start().await;
    let ops_srv = MockServer::start().await;
    for s in [&default_srv, &ops_srv] {
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(204))
            .mount(s)
            .await;
    }
    let mut channels: HashMap<String, Arc<dyn Channel>> = HashMap::new();
    channels.insert(
        "default".into(),
        Arc::new(DiscordWebhookChannel::new(default_srv.uri())),
    );
    channels.insert(
        "ops".into(),
        Arc::new(DiscordWebhookChannel::new(ops_srv.uri())),
    );

    // The schema surfaces the configured channel names as an enum, so the
    // model picks a valid channel instead of guessing.
    let schema = NotifyTool::new(channels.clone()).parameters_schema();
    let enum_names = schema["properties"]["channel"]["enum"]
        .as_array()
        .expect("channel enum");
    assert!(enum_names.iter().any(|n| n == "default"));
    assert!(enum_names.iter().any(|n| n == "ops"));

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(NotifyTool::new(channels)));
    let member = ToolContext {
        workspace_id: Some(WorkspaceId::new()),
        capabilities: Some(catalerum_iam::base_capabilities(Role::Member)),
        ..Default::default()
    };

    // `channel: "ops"` routes to the ops server only.
    let out = registry
        .dispatch(
            "notify",
            json!({ "message": "ship it", "channel": "ops" }),
            &member,
        )
        .await
        .unwrap();
    assert_eq!(out["channel"], json!("ops"));
    assert_eq!(ops_srv.received_requests().await.unwrap().len(), 1);
    assert_eq!(
        default_srv.received_requests().await.unwrap().len(),
        0,
        "ops message didn't go to default"
    );
    let body: Json =
        serde_json::from_slice(&ops_srv.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(body, json!({ "content": "ship it" }));

    // No `channel` → the `default` channel.
    registry
        .dispatch("notify", json!({ "message": "hi" }), &member)
        .await
        .unwrap();
    assert_eq!(default_srv.received_requests().await.unwrap().len(), 1);

    // An unknown channel → a clear error, and nothing is sent.
    let err = registry
        .dispatch(
            "notify",
            json!({ "message": "x", "channel": "nope" }),
            &member,
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("unknown channel"));
    assert!(
        err.to_string().contains("default") && err.to_string().contains("ops"),
        "lists configured channels"
    );

    // A Viewer lacks channel:write → denied deny-by-default; no extra delivery.
    let viewer = ToolContext {
        workspace_id: Some(WorkspaceId::new()),
        capabilities: Some(catalerum_iam::base_capabilities(Role::Viewer)),
        ..Default::default()
    };
    assert!(registry
        .dispatch("notify", json!({ "message": "x" }), &viewer)
        .await
        .is_err());
    assert_eq!(
        default_srv.received_requests().await.unwrap().len(),
        1,
        "denied notify never delivers"
    );
}

#[test]
fn required_str_rejects_blank_and_missing() {
    assert!(required_str(&json!({ "title": "  " }), "title").is_err());
    assert!(required_str(&json!({}), "title").is_err());
    assert_eq!(
        required_str(&json!({ "title": " Hi " }), "title").unwrap(),
        "Hi"
    );
}

#[test]
fn source_ref_parses_endpoint_and_rejects_bad_shape() {
    let id = catalerum_core::NoteId::new();
    let args = json!({ "from": { "kind": "note", "id": id.to_string() } });
    assert_eq!(source_ref(&args, "from").unwrap(), SourceRef::Note { id });
    // An external endpoint carries a uri, not a uuid.
    let ext = json!({ "e": { "kind": "external", "id": "https://example.com" } });
    assert_eq!(
        source_ref(&ext, "e").unwrap(),
        SourceRef::External {
            uri: "https://example.com".into()
        }
    );
    // Missing key / kind / id, unknown kind, and a non-uuid first-class id all error.
    assert!(source_ref(&json!({}), "from").is_err());
    assert!(source_ref(&json!({ "from": { "id": "x" } }), "from").is_err());
    assert!(source_ref(&json!({ "from": { "kind": "note" } }), "from").is_err());
    assert!(source_ref(&json!({ "from": { "kind": "bogus", "id": "x" } }), "from").is_err());
    assert!(source_ref(
        &json!({ "from": { "kind": "note", "id": "not-a-uuid" } }),
        "from"
    )
    .is_err());
}

#[test]
fn note_id_parses_and_rejects() {
    let id = NoteId::new();
    let parsed = note_id(&json!({ "id": id.to_string() })).unwrap();
    assert_eq!(parsed, id);
    assert!(note_id(&json!({ "id": "not-a-uuid" })).is_err());
}

#[test]
fn workspace_and_author_require_context() {
    let empty = ToolContext::default();
    assert!(workspace(&empty).is_err());
    assert!(author(&empty).is_err());

    let user = catalerum_core::UserId::new();
    let ws = WorkspaceId::new();
    let ctx = ToolContext {
        workspace_id: Some(ws),
        user_id: Some(user),
        ..Default::default()
    };
    assert_eq!(workspace(&ctx).unwrap(), ws);
    assert!(matches!(author(&ctx).unwrap(), Author::User { id } if id == user));

    let agent = catalerum_core::AgentId::new();
    let ctx = ToolContext {
        workspace_id: Some(ws),
        agent_id: Some(agent),
        ..Default::default()
    };
    // An agent run authors as the agent even if a user is also present.
    assert!(matches!(author(&ctx).unwrap(), Author::Agent { id } if id == agent));
}

#[test]
fn opt_str_vec_trims_drops_empty_and_handles_absent() {
    assert_eq!(
        opt_str_vec(&json!({ "kinds": [" note ", "", "memory", "  "] }), "kinds"),
        vec!["note".to_string(), "memory".to_string()]
    );
    assert!(opt_str_vec(&json!({}), "kinds").is_empty());
    assert!(opt_str_vec(&json!({ "kinds": "note" }), "kinds").is_empty());
}

#[test]
fn opt_clamped_u64_clamps_and_defaults() {
    assert_eq!(opt_clamped_u64(&json!({}), "limit", 8, 20), 8);
    assert_eq!(opt_clamped_u64(&json!({ "limit": 3 }), "limit", 8, 20), 3);
    assert_eq!(
        opt_clamped_u64(&json!({ "limit": 999 }), "limit", 8, 20),
        20
    );
    assert_eq!(opt_clamped_u64(&json!({ "limit": 0 }), "limit", 8, 20), 1);
}

// --- live search_semantic test (QDRANT-gated, skip-and-pass offline) -----

const DIM: u64 = 8;

/// Deterministic fake embedder: identical text → identical `DIM`-wide vector.
struct FakeEmbedder;

fn fake_vec(text: &str) -> Vec<f32> {
    let seed = text.bytes().fold(1469598103934665603u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(1099511628211)
    });
    (0..DIM)
        .map(|i| (((seed >> (i * 4)) & 0xF) as f32) + 1.0)
        .collect()
}

#[async_trait]
impl Embedder for FakeEmbedder {
    async fn embed(
        &self,
        request: catalerum_core::embed::EmbeddingRequest,
    ) -> Result<catalerum_core::embed::EmbeddingResponse> {
        let embeddings = request
            .input
            .iter()
            .enumerate()
            .map(|(i, t)| catalerum_core::embed::Embedding {
                index: i as u32,
                vector: fake_vec(t),
            })
            .collect();
        Ok(catalerum_core::embed::EmbeddingResponse {
            model: request.model,
            embeddings,
            usage: None,
        })
    }
}

fn qdrant_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_QDRANT_URL")
        .or_else(|_| std::env::var("QDRANT_URL"))
        .ok()
}

#[tokio::test]
async fn search_semantic_returns_a_seeded_hit_scoped_to_workspace() {
    let (Some(qurl), Some(db)) = (qdrant_url(), db_url()) else {
        eprintln!("skipping search_semantic test: set QDRANT_URL and CATALERUM_TEST_DATABASE_URL");
        return;
    };
    use catalerum_vector::{PointPayload, VectorPoint};

    let vector = VectorStore::new(&qurl).expect("qdrant client");
    let store = Store::connect(&db).await.expect("store");
    let ws = WorkspaceId::new();
    let other = WorkspaceId::new();
    let _ = vector.delete_collection(ws).await;
    let _ = vector.delete_collection(other).await;

    // Seed one point whose embedding equals fake_vec("the quarterly roadmap").
    let text = "the quarterly roadmap";
    let src = SourceRef::Note { id: NoteId::new() };
    vector.ensure_collection(ws, DIM).await.expect("ensure");
    vector
        .upsert(
            ws,
            &[VectorPoint::new(
                fake_vec(text),
                PointPayload::new(ws, src.clone(), text),
            )],
        )
        .await
        .expect("seed point");

    let tool = SearchSemanticTool {
        search: SemanticSearch {
            embedder: Arc::new(FakeEmbedder),
            vector: vector.clone(),
            embed_model: "fake".into(),
        },
        store,
    };

    // A query with the same text embeds to the same vector → top hit.
    let ctx = ToolContext {
        workspace_id: Some(ws),
        ..Default::default()
    };
    let out = tool
        .invoke(json!({ "query": text, "limit": 3 }), &ctx)
        .await
        .expect("invoke");
    let hits = out["hits"].as_array().expect("hits array");
    assert!(!hits.is_empty(), "the seeded chunk is retrieved");
    assert_eq!(hits[0]["text"], json!(text));
    assert_eq!(hits[0]["source"]["kind"], json!("note"));

    // Another workspace sees nothing (workspace-scoped, §18).
    let ctx_other = ToolContext {
        workspace_id: Some(other),
        ..Default::default()
    };
    let out_other = tool
        .invoke(json!({ "query": text }), &ctx_other)
        .await
        .expect("invoke other");
    assert!(out_other["hits"].as_array().unwrap().is_empty());

    // Missing workspace is rejected (no cross-workspace default).
    assert!(tool
        .invoke(json!({ "query": text }), &ToolContext::default())
        .await
        .is_err());

    let _ = vector.delete_collection(ws).await;
    let _ = vector.delete_collection(other).await;
}

#[tokio::test]
async fn search_semantic_overfetches_past_filtered_email_hits() {
    let (Some(qurl), Some(db)) = (qdrant_url(), db_url()) else {
        eprintln!(
            "skipping search_semantic_overfetches: set QDRANT_URL and CATALERUM_TEST_DATABASE_URL"
        );
        return;
    };
    use catalerum_vector::{PointPayload, VectorPoint};

    let vector = VectorStore::new(&qurl).expect("qdrant client");
    let store = Store::connect(&db).await.expect("store");
    let ws = WorkspaceId::new();
    let _ = vector.delete_collection(ws).await;
    vector.ensure_collection(ws, DIM).await.expect("ensure");

    // Five email points whose vectors equal the query occupy the top ranks and
    // are all dropped by the email filter; one note ranks below them. Without
    // over-fetching, a small `limit` would return nothing (the top-`limit` hits
    // are all filtered emails); over-fetch surfaces the note past them.
    let query = "deployment status report";
    let note_text = "an unrelated planning note";
    let mut points: Vec<VectorPoint> = (0..5)
        .map(|_| {
            VectorPoint::new(
                fake_vec(query),
                PointPayload::new(
                    ws,
                    SourceRef::Email {
                        id: catalerum_core::EmailId::new(),
                    },
                    query,
                ),
            )
        })
        .collect();
    points.push(VectorPoint::new(
        fake_vec(note_text),
        PointPayload::new(ws, SourceRef::Note { id: NoteId::new() }, note_text),
    ));
    vector.upsert(ws, &points).await.expect("seed");

    let tool = SearchSemanticTool {
        search: SemanticSearch {
            embedder: Arc::new(FakeEmbedder),
            vector: vector.clone(),
            embed_model: "fake".into(),
        },
        store,
    };
    let ctx = ToolContext {
        workspace_id: Some(ws),
        ..Default::default()
    };
    // limit=2: the two top hits are filtered emails; the note (rank 6) survives
    // only because we over-fetch before filtering, then truncate to `limit`.
    let out = tool
        .invoke(json!({ "query": query, "limit": 2 }), &ctx)
        .await
        .expect("invoke");
    let hits = out["hits"].as_array().expect("hits");
    assert_eq!(
        hits.len(),
        1,
        "the note survives past the filtered emails: {hits:?}"
    );
    assert_eq!(hits[0]["source"]["kind"], json!("note"));
    assert_eq!(hits[0]["text"], json!(note_text));

    let _ = vector.delete_collection(ws).await;
}

// --- live query_graph test (NEO4J-gated, skip-and-pass offline) ----------

fn graph_store() -> Option<GraphStore> {
    let url = std::env::var("NEO4J_URL").ok()?;
    let user = std::env::var("NEO4J_USER").unwrap_or_else(|_| "neo4j".into());
    let password = std::env::var("NEO4J_PASSWORD").unwrap_or_else(|_| "catalerum".into());
    Some(
        GraphStore::new(&url)
            .expect("NEO4J_URL")
            .with_auth(user, password),
    )
}

#[tokio::test]
async fn query_graph_related_notes_and_notes_by_topic_scoped_to_workspace() {
    let Some(graph) = graph_store() else {
        eprintln!("skipping query_graph test: set NEO4J_URL");
        return;
    };
    use catalerum_core::model::Author;
    use catalerum_core::{Entity, EntityKind, Note, UserId};

    graph.ensure_indexes().await.expect("indexes");
    let ws = WorkspaceId::new();
    graph.delete_workspace(ws).await.unwrap();

    let topic = Entity {
        id: catalerum_core::EntityId::new(),
        workspace_id: ws,
        kind: EntityKind::Topic,
        display_name: "Scheduling".into(),
        aliases: vec![],
    };
    let mk = |title: &str| Note {
        id: NoteId::new(),
        workspace_id: ws,
        author: Author::User { id: UserId::new() },
        title: title.into(),
        markdown: String::new(),
        tags: vec![],
        updated_at: chrono::Utc::now(),
    };
    let n1 = mk("Sprint plan");
    let n2 = mk("Roadmap");
    graph
        .project_note(&n1, std::slice::from_ref(&topic))
        .await
        .unwrap();
    graph
        .project_note(&n2, std::slice::from_ref(&topic))
        .await
        .unwrap();

    let tool = QueryGraphTool {
        graph: GraphQuery::Neo4j(graph.clone()),
    };
    let ctx = ToolContext {
        workspace_id: Some(ws),
        ..Default::default()
    };

    // related_notes(n1) → n2.
    let related = tool
        .invoke(
            json!({ "operation": "related_notes", "note_id": n1.id.to_string() }),
            &ctx,
        )
        .await
        .expect("related_notes");
    let results = related["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["note_id"], json!(n2.id.to_string()));
    assert_eq!(results[0]["shared_topics"], json!(1));

    // notes_by_topic("scheduling") → both notes (case-insensitive).
    let by_topic = tool
        .invoke(
            json!({ "operation": "notes_by_topic", "topic": "scheduling" }),
            &ctx,
        )
        .await
        .expect("notes_by_topic");
    assert_eq!(by_topic["results"].as_array().unwrap().len(), 2);

    // Workspace isolation: another workspace sees nothing.
    let other = ToolContext {
        workspace_id: Some(WorkspaceId::new()),
        ..Default::default()
    };
    let none = tool
        .invoke(
            json!({ "operation": "notes_by_topic", "topic": "scheduling" }),
            &other,
        )
        .await
        .unwrap();
    assert!(none["results"].as_array().unwrap().is_empty());

    // Bad inputs: unknown operation, missing workspace, bad note id.
    assert!(tool
        .invoke(json!({ "operation": "wat" }), &ctx)
        .await
        .is_err());
    assert!(tool
        .invoke(
            json!({ "operation": "related_notes", "note_id": n1.id.to_string() }),
            &ToolContext::default()
        )
        .await
        .is_err());
    assert!(tool
        .invoke(
            json!({ "operation": "related_notes", "note_id": "nope" }),
            &ctx
        )
        .await
        .is_err());

    graph.delete_workspace(ws).await.unwrap();
}

// --- live query_structured test (DB-gated, skip-and-pass offline) --------

fn db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

/// `write_object` + `move_object` round-trip against a real local backend
/// (SOUL §9/§11): write text, read it back byte-identical, move it, and the
/// bytes exist only at the destination. Also pins the arg validation (content
/// XOR content_base64, size cap) and the `storage:write` gates. DB-gated (the
/// write path catalogues + fires the storage trigger through the store).
#[tokio::test]
async fn write_and_move_object_tools_round_trip() {
    let Some(url) = db_url() else {
        eprintln!(
            "skipping write/move object test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
        );
        return;
    };
    use crate::state::ConfigStore;
    use catalerum_storage::LocalFsBackend;

    let store_db = Store::connect(&url).await.expect("store");
    let ws = store_db
        .workspaces()
        .create("wrmv", &format!("wrmv-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let tmp = tempfile::tempdir().expect("tmp");
    let storage = crate::state::StorageRegistry::single_for_test(
        "files",
        ConfigStore {
            backend: Arc::new(LocalFsBackend::new(tmp.path().to_path_buf())),
            connection: "files".to_string(),
            bucket: "files".to_string(),
            kind: "local",
            namespaced: true,
            workspaces: Vec::new(),
        },
    );
    let write = WriteObjectTool {
        storage: storage.clone(),
        store: store_db.clone(),
    };
    let mv = MoveObjectTool {
        storage: storage.clone(),
        store: store_db.clone(),
    };
    // Both are side-effecting storage ops → `storage:write`, like copy/delete.
    assert_eq!(write.required_capability(), cap(Action::Write, "storage"));
    assert_eq!(mv.required_capability(), cap(Action::Write, "storage"));

    let ctx = ToolContext {
        workspace_id: Some(ws.id),
        ..Default::default()
    };

    // Write text → read back byte-identical (workspace-namespaced under the hood).
    let out = write
        .invoke(
            json!({ "key": "reports/note.md", "content": "hello *world*", "content_type": "text/markdown" }),
            &ctx,
        )
        .await
        .expect("write");
    assert_eq!(out["key"], json!("reports/note.md"));
    assert_eq!(out["size"], json!(13));
    let (bytes, _) = crate::routes::storage::read_object_bytes(
        &storage,
        &store_db,
        ws.id,
        None,
        (None, "reports/note.md"),
    )
    .await
    .expect("read back");
    assert_eq!(bytes, b"hello *world*");

    // Binary via base64 (and overwrite-by-key is fine — same key, new bytes).
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode([0u8, 159, 146, 150]);
    write
        .invoke(json!({ "key": "blob.bin", "content_base64": b64 }), &ctx)
        .await
        .expect("binary write");
    let (bytes, _) = crate::routes::storage::read_object_bytes(
        &storage,
        &store_db,
        ws.id,
        None,
        (None, "blob.bin"),
    )
    .await
    .expect("read blob");
    assert_eq!(bytes, [0u8, 159, 146, 150]);

    // Arg validation: content XOR content_base64; bad base64 is a clear error.
    for bad in [
        json!({ "key": "x", "content": "a", "content_base64": "YQ==" }),
        json!({ "key": "x" }),
        json!({ "key": "x", "content_base64": "not base64!!" }),
    ] {
        let err = write.invoke(bad, &ctx).await.unwrap_err();
        assert!(matches!(err, Error::Invalid(_)), "got {err:?}");
    }

    // Move: bytes land at the destination and the source is gone.
    let out = mv
        .invoke(
            json!({ "from_key": "reports/note.md", "to_key": "archive/note.md" }),
            &ctx,
        )
        .await
        .expect("move");
    assert_eq!(out["moved"], json!(true));
    assert_eq!(out["key"], json!("archive/note.md"));
    let (bytes, _) = crate::routes::storage::read_object_bytes(
        &storage,
        &store_db,
        ws.id,
        None,
        (None, "archive/note.md"),
    )
    .await
    .expect("read moved");
    assert_eq!(bytes, b"hello *world*");
    assert!(
        crate::routes::storage::read_object_bytes(
            &storage,
            &store_db,
            ws.id,
            None,
            (None, "reports/note.md"),
        )
        .await
        .is_err(),
        "the source must be deleted after a move"
    );

    // Moving a missing source is a clear error (the copy half 404s first).
    assert!(mv
        .invoke(
            json!({ "from_key": "reports/note.md", "to_key": "again.md" }),
            &ctx
        )
        .await
        .is_err());
}

#[tokio::test]
async fn index_document_enqueues_per_source_and_rejects_bad_input() {
    let Some(url) = db_url() else {
        eprintln!("skipping index_document test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    use catalerum_ingest::{JOB_KIND_INGEST_MEMORY, JOB_KIND_INGEST_NOTE, JOB_KIND_INGEST_OBJECT};

    let store = Store::connect(&url).await.expect("store");
    let ws = store
        .workspaces()
        .create("idxdoc", &format!("idxdoc-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let user = UserId::new();
    let tool = IndexDocumentTool {
        store: store.clone(),
        // A non-connecting client (the id-based paths exercised here never touch
        // Qdrant; only the bucket+key delete path would).
        vector: catalerum_vector::VectorStore::new("http://localhost:6333").unwrap(),
    };

    // Static metadata: `index_document`, gated on `vector:write` (so a Viewer is
    // denied), with a closed `source` enum.
    assert_eq!(tool.name(), "index_document");
    assert_eq!(tool.required_capability(), cap(Action::Write, "vector"));
    let schema = tool.parameters_schema();
    assert_eq!(
        schema["properties"]["source"]["enum"],
        json!(["object", "note", "memory"])
    );

    let ctx = ToolContext {
        workspace_id: Some(ws.id),
        user_id: Some(user),
        ..Default::default()
    };

    // A real note → indexing it enqueues a durable `ingest_note` job scoped to
    // the workspace; the tool returns the queued job id (the embed runs in the
    // worker, not here).
    let note = store
        .notes()
        .create(ws.id, Author::User { id: user }, "Indexable", "body", &[])
        .await
        .expect("note");
    let out = tool
        .invoke(json!({ "source": "note", "id": note.id.to_string() }), &ctx)
        .await
        .expect("index note");
    assert_eq!(out["enqueued"], json!(true));
    assert_eq!(out["source"], json!("note"));
    let job_id: uuid::Uuid = out["job_id"].as_str().unwrap().parse().unwrap();
    let job = store.job_queue().get(job_id).await.expect("job row");
    assert_eq!(job.kind, JOB_KIND_INGEST_NOTE);
    assert_eq!(job.workspace_id(), Some(ws.id));

    // The `source` discriminant is case-insensitive, and memory/object route to
    // their own ingest jobs (the id needn't resolve to a row — the durable job is
    // reconciled later by the worker, purging if the source is gone).
    let out = tool
        .invoke(
            json!({ "source": "MEMORY", "id": MemoryId::new().to_string() }),
            &ctx,
        )
        .await
        .expect("index memory");
    let job = store
        .job_queue()
        .get(out["job_id"].as_str().unwrap().parse().unwrap())
        .await
        .expect("job");
    assert_eq!(job.kind, JOB_KIND_INGEST_MEMORY);

    let out = tool
        .invoke(
            json!({ "source": "object", "id": ObjectId::new().to_string() }),
            &ctx,
        )
        .await
        .expect("index object");
    let job = store
        .job_queue()
        .get(out["job_id"].as_str().unwrap().parse().unwrap())
        .await
        .expect("job");
    assert_eq!(job.kind, JOB_KIND_INGEST_OBJECT);

    // Unknown source kind, a malformed id, and missing fields are all caller
    // errors (no job enqueued).
    assert!(tool
        .invoke(json!({ "source": "calendar", "id": "x" }), &ctx)
        .await
        .is_err());
    assert!(tool
        .invoke(json!({ "source": "note", "id": "not-a-uuid" }), &ctx)
        .await
        .is_err());
    assert!(tool
        .invoke(json!({ "source": "note" }), &ctx)
        .await
        .is_err());
}

#[tokio::test]
async fn query_structured_enforces_per_domain_read_capability() {
    let Some(url) = db_url() else {
        eprintln!(
            "skipping query_structured cap test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
        );
        return;
    };
    use catalerum_core::capability::{Capability, Resource};

    let store = Store::connect(&url).await.expect("store");
    let ws = store
        .workspaces()
        .create("qscap", &format!("qscap-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let tool = QueryStructuredTool {
        store: store.clone(),
        storage: None,
    };

    // A caller scoped to ONLY notes:read.
    let notes_only = ToolContext {
        workspace_id: Some(ws.id),
        capabilities: Some(vec![Capability::new(
            Action::Read,
            Resource::domain("notes"),
        )]),
        ..Default::default()
    };
    // A notes op is permitted; reaching calendar/tasks/storage data is denied.
    assert!(tool
        .invoke(json!({ "operation": "recent_notes" }), &notes_only)
        .await
        .is_ok());
    for op in [
        "upcoming_events",
        "open_tasks",
        "boards",
        "calendars",
        "recent_objects",
        "unlabeled_objects",
    ] {
        let err = tool
            .invoke(json!({ "operation": op }), &notes_only)
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::Unauthorized(_)),
            "notes-only must not reach `{op}`, got {err:?}"
        );
    }

    // A calendar:read caller can run calendar ops but not notes ops.
    let cal_only = ToolContext {
        workspace_id: Some(ws.id),
        capabilities: Some(vec![Capability::new(
            Action::Read,
            Resource::domain("calendar"),
        )]),
        ..Default::default()
    };
    assert!(tool
        .invoke(json!({ "operation": "calendars" }), &cal_only)
        .await
        .is_ok());
    assert!(matches!(
        tool.invoke(json!({ "operation": "recent_notes" }), &cal_only)
            .await
            .unwrap_err(),
        Error::Unauthorized(_)
    ));

    // An unscoped caller (no capabilities) may run any op (full authority).
    let unscoped = ToolContext {
        workspace_id: Some(ws.id),
        ..Default::default()
    };
    assert!(tool
        .invoke(json!({ "operation": "upcoming_events" }), &unscoped)
        .await
        .is_ok());
}

#[tokio::test]
async fn query_structured_object_operations_carry_store_and_labels() {
    let Some(url) = db_url() else {
        eprintln!("skipping query_structured objects test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    use catalerum_core::model::{Author, ConnectionKind};
    use catalerum_core::UserId;
    use catalerum_store::UpsertObject;

    let store = Store::connect(&url).await.expect("store");
    let ws = store
        .workspaces()
        .create("qsobj", &format!("qsobj-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let conn = store
        .connections()
        .create(ws.id, ConnectionKind::Storage, "files", None, None)
        .await
        .expect("connection");
    let bucket = store
        .buckets()
        .ensure(ws.id, conn.id, "default", None)
        .await
        .expect("bucket");
    let now = chrono::Utc::now();
    for key in ["docs/a.md", "docs/b.md", "other/c.md"] {
        store
            .objects()
            .upsert(&UpsertObject {
                workspace_id: ws.id,
                bucket_id: bucket.id,
                key,
                size: 1,
                content_type: Some("text/plain"),
                etag: None,
                last_modified: now,
                sha256: None,
            })
            .await
            .expect("object");
    }
    // Label one file under the store name the `storage: None` fallback
    // resolves (the connection name — runtime-store naming).
    store
        .object_labels()
        .add(
            ws.id,
            Author::User { id: UserId::new() },
            "files",
            "docs/a.md",
            false,
            "work",
        )
        .await
        .expect("label");

    let tool = QueryStructuredTool {
        store: store.clone(),
        storage: None,
    };
    let ctx = ToolContext {
        workspace_id: Some(ws.id),
        ..Default::default()
    };

    // recent_objects → summaries carry store + labels (the labelled file its
    // set, the rest an empty array — never a missing field).
    let recent = tool
        .invoke(json!({ "operation": "recent_objects" }), &ctx)
        .await
        .unwrap();
    let rows = recent["results"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|r| r["store"] == json!("files")));
    let labelled = rows
        .iter()
        .find(|r| r["key"] == json!("docs/a.md"))
        .expect("a.md listed");
    assert_eq!(labelled["labels"], json!(["work"]));
    assert!(rows
        .iter()
        .filter(|r| r["key"] != json!("docs/a.md"))
        .all(|r| r["labels"] == json!([])));

    // unlabeled_objects → the labelled file drops out; `prefix` narrows the
    // sweep to one subdirectory.
    let unlabeled = tool
        .invoke(json!({ "operation": "unlabeled_objects" }), &ctx)
        .await
        .unwrap();
    let u = unlabeled["results"].as_array().unwrap();
    assert_eq!(u.len(), 2, "a.md is labelled");
    assert!(u.iter().all(|r| r["labels"] == json!([])));
    let docs_only = tool
        .invoke(
            json!({ "operation": "unlabeled_objects", "prefix": "docs/" }),
            &ctx,
        )
        .await
        .unwrap();
    let d = docs_only["results"].as_array().unwrap();
    assert_eq!(d.len(), 1);
    assert_eq!(d[0]["key"], json!("docs/b.md"));

    // §18: another workspace sees nothing.
    let other = ToolContext {
        workspace_id: Some(WorkspaceId::new()),
        ..Default::default()
    };
    let none = tool
        .invoke(json!({ "operation": "unlabeled_objects" }), &other)
        .await
        .unwrap();
    assert!(none["results"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn query_structured_notes_operations_are_workspace_scoped() {
    let Some(url) = db_url() else {
        eprintln!(
            "skipping query_structured test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
        );
        return;
    };
    use catalerum_core::model::Author;
    use catalerum_core::UserId;

    let store = Store::connect(&url).await.expect("store");
    let ws = store
        .workspaces()
        .create("qs", &format!("qs-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let author = Author::User { id: UserId::new() };
    store
        .notes()
        .create(ws.id, author, "Groceries", "milk", &["home".into()])
        .await
        .unwrap();
    store
        .notes()
        .create(ws.id, author, "Standup", "notes", &["Work".into()])
        .await
        .unwrap();

    let tool = QueryStructuredTool {
        store: store.clone(),
        storage: None,
    };
    let ctx = ToolContext {
        workspace_id: Some(ws.id),
        ..Default::default()
    };

    // recent_notes returns both, most-recent first (Standup created last).
    let recent = tool
        .invoke(json!({ "operation": "recent_notes" }), &ctx)
        .await
        .unwrap();
    let rows = recent["results"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["title"], json!("Standup"));

    // notes_by_tag is case-insensitive ("WORK" matches tag "Work").
    let by_tag = tool
        .invoke(json!({ "operation": "notes_by_tag", "tag": "WORK" }), &ctx)
        .await
        .unwrap();
    let tagged = by_tag["results"].as_array().unwrap();
    assert_eq!(tagged.len(), 1);
    assert_eq!(tagged[0]["title"], json!("Standup"));

    // upcoming_events runs (no events yet → empty), exercising the events path.
    let events = tool
        .invoke(json!({ "operation": "upcoming_events" }), &ctx)
        .await
        .unwrap();
    assert!(events["results"].as_array().unwrap().is_empty());

    // Another workspace sees none of these notes (§18).
    let other = ToolContext {
        workspace_id: Some(WorkspaceId::new()),
        ..Default::default()
    };
    let none = tool
        .invoke(json!({ "operation": "recent_notes" }), &other)
        .await
        .unwrap();
    assert!(none["results"].as_array().unwrap().is_empty());

    // Bad inputs: unknown operation, missing workspace, missing tag.
    assert!(tool
        .invoke(json!({ "operation": "wat" }), &ctx)
        .await
        .is_err());
    assert!(tool
        .invoke(
            json!({ "operation": "recent_notes" }),
            &ToolContext::default()
        )
        .await
        .is_err());
    assert!(tool
        .invoke(json!({ "operation": "notes_by_tag" }), &ctx)
        .await
        .is_err());
}

#[tokio::test]
async fn query_structured_events_in_range_filters_to_the_window() {
    let Some(url) = db_url() else {
        eprintln!(
            "skipping query_structured events test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
        );
        return;
    };
    let store = Store::connect(&url).await.expect("store");
    let ws = store
        .workspaces()
        .create("qsev", &format!("qsev-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let cal = store
        .calendars()
        .upsert_local(ws.id, "default", "Calendar")
        .await
        .unwrap();

    // Two events, two months apart.
    for (uid, summary, start, end) in [
        (
            "jan",
            "Kickoff",
            "2026-01-15T09:00:00Z",
            "2026-01-15T10:00:00Z",
        ),
        (
            "mar",
            "Launch",
            "2026-03-20T09:00:00Z",
            "2026-03-20T10:00:00Z",
        ),
    ] {
        store
            .events()
            .create(&UpsertEvent {
                workspace_id: ws.id,
                calendar_id: cal.id,
                uid,
                starts_at: start.parse().unwrap(),
                ends_at: end.parse().unwrap(),
                all_day: false,
                rrule: None,
                summary,
                location: None,
                body: None,
                attendees: &[],
                labels: &[],
                attachments: &[],
                etag: None,
                sequence: 0,
            })
            .await
            .unwrap();
    }

    let tool = QueryStructuredTool {
        store: store.clone(),
        storage: None,
    };
    let ctx = ToolContext {
        workspace_id: Some(ws.id),
        ..Default::default()
    };

    // A January window catches only "Kickoff".
    let jan = tool
        .invoke(
            json!({ "operation": "events_in_range", "from": "2026-01-01T00:00:00Z", "to": "2026-02-01T00:00:00Z" }),
            &ctx,
        )
        .await
        .unwrap();
    let rows = jan["results"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["summary"], json!("Kickoff"));

    // A full-year window catches both.
    let year = tool
        .invoke(
            json!({ "operation": "events_in_range", "from": "2026-01-01T00:00:00Z", "to": "2026-12-31T00:00:00Z" }),
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(year["results"].as_array().unwrap().len(), 2);

    // `to` before `from` is rejected, and `from`/`to` are required.
    assert!(tool
        .invoke(
            json!({ "operation": "events_in_range", "from": "2026-03-01T00:00:00Z", "to": "2026-01-01T00:00:00Z" }),
            &ctx,
        )
        .await
        .is_err());
    assert!(tool
        .invoke(
            json!({ "operation": "events_in_range", "from": "2026-01-01T00:00:00Z" }),
            &ctx
        )
        .await
        .is_err());

    // calendars lists the workspace's calendars; the local "default" one is
    // writable (what create_event needs) — the agent's calendar_id source.
    let cals = tool
        .invoke(json!({ "operation": "calendars" }), &ctx)
        .await
        .unwrap();
    let cal_list = cals["results"].as_array().unwrap();
    let default = cal_list
        .iter()
        .find(|c| c["id"] == json!(cal.id))
        .expect("the default calendar is listed");
    assert_eq!(default["name"], json!("Calendar"));
    assert_eq!(default["local"], json!(true));
    assert_eq!(default["writable"], json!(true));
}

#[tokio::test]
async fn read_event_returns_full_detail_beyond_the_summary() {
    let Some(url) = db_url() else {
        eprintln!("skipping read_event test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    let store = Store::connect(&url).await.expect("store");
    let ws = store
        .workspaces()
        .create("readev", &format!("readev-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let cal = store
        .calendars()
        .upsert_local(ws.id, "default", "Calendar")
        .await
        .unwrap();
    let event = store
        .events()
        .create(&UpsertEvent {
            workspace_id: ws.id,
            calendar_id: cal.id,
            uid: "rev",
            starts_at: "2026-05-01T09:00:00Z".parse().unwrap(),
            ends_at: "2026-05-01T10:00:00Z".parse().unwrap(),
            all_day: false,
            rrule: Some("FREQ=WEEKLY"),
            summary: "Standup",
            location: Some("Room 1"),
            body: Some("Agenda: ship it"),
            attendees: &[],
            labels: &["planning".to_string()],
            attachments: &[Attachment {
                url: "https://example.com/deck.pdf".to_string(),
                filename: Some("deck.pdf".to_string()),
                content_type: Some("application/pdf".to_string()),
                size: None,
            }],
            etag: None,
            sequence: 0,
        })
        .await
        .unwrap();

    let tool = ReadEventTool {
        store: store.clone(),
    };
    let ctx = ToolContext {
        workspace_id: Some(ws.id),
        ..Default::default()
    };

    // read_event surfaces the body + rrule the query_structured summary omits.
    let out = tool
        .invoke(json!({ "id": event.id }), &ctx)
        .await
        .expect("read_event");
    assert_eq!(out["summary"], json!("Standup"));
    assert_eq!(out["location"], json!("Room 1"));
    assert_eq!(out["body"], json!("Agenda: ship it"));
    assert_eq!(out["rrule"], json!("FREQ=WEEKLY"));
    assert!(out["attendees"].is_array());
    // read_event surfaces labels + attachments too.
    assert_eq!(out["labels"], json!(["planning"]));
    assert_eq!(
        out["attachments"][0]["url"],
        json!("https://example.com/deck.pdf")
    );
    assert_eq!(out["attachments"][0]["filename"], json!("deck.pdf"));

    // A bad id errors; another workspace can't read this event (§18 — NotFound).
    assert!(tool.invoke(json!({ "id": "nope" }), &ctx).await.is_err());
    let other = ToolContext {
        workspace_id: Some(WorkspaceId::new()),
        ..Default::default()
    };
    assert!(
        tool.invoke(json!({ "id": event.id }), &other)
            .await
            .is_err(),
        "another workspace cannot read this event"
    );
}

#[tokio::test]
async fn read_task_tool_returns_full_detail_with_body() {
    let Some(url) = db_url() else {
        eprintln!("skipping read_task test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    let store = Store::connect(&url).await.expect("store");
    let ws = store
        .workspaces()
        .create("readtask", &format!("readtask-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let board = store
        .boards()
        .create(ws.id, "Sprint", &[])
        .await
        .expect("board");
    let col = board.columns.first().expect("a default column").clone();
    let task = store
        .tasks()
        .create(
            ws.id,
            board.id,
            col.id,
            "Ship read_task",
            "Wire it + a test.",
            None,
        )
        .await
        .expect("task");

    let tool = ReadTaskTool {
        store: store.clone(),
    };
    let ctx = ToolContext {
        workspace_id: Some(ws.id),
        ..Default::default()
    };

    // read_task surfaces the body the query_structured summary omits, with the
    // board/column names resolved.
    let out = tool
        .invoke(json!({ "id": task.id }), &ctx)
        .await
        .expect("kanban_read_task");
    assert_eq!(out["title"], json!("Ship read_task"));
    assert_eq!(out["body"], json!("Wire it + a test."));
    assert_eq!(out["board"], json!("Sprint"));
    assert_eq!(out["column"], json!(col.name));
    assert!(out.get("status").is_some(), "status is carried");

    // A bad id errors; another workspace can't read this task (§18 — NotFound).
    assert!(tool.invoke(json!({ "id": "nope" }), &ctx).await.is_err());
    let other = ToolContext {
        workspace_id: Some(WorkspaceId::new()),
        ..Default::default()
    };
    assert!(
        tool.invoke(json!({ "id": task.id }), &other).await.is_err(),
        "another workspace cannot read this task"
    );
}

#[tokio::test]
async fn search_tasks_tool_finds_by_title_or_body() {
    let Some(url) = db_url() else {
        eprintln!("skipping search_tasks test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    let store = Store::connect(&url).await.expect("store");
    let ws = store
        .workspaces()
        .create(
            "searchtask",
            &format!("searchtask-{}", uuid::Uuid::new_v4()),
        )
        .await
        .expect("ws");
    let board = store
        .boards()
        .create(ws.id, "Sprint", &[])
        .await
        .expect("board");
    let col = board.columns.first().expect("a column").clone();
    store
        .tasks()
        .create(ws.id, board.id, col.id, "Migrate the database", "", None)
        .await
        .expect("t1");
    store
        .tasks()
        .create(ws.id, board.id, col.id, "Lunch", "nothing relevant", None)
        .await
        .expect("t2");

    let tool = SearchTasksTool {
        store: store.clone(),
    };
    let ctx = ToolContext {
        workspace_id: Some(ws.id),
        ..Default::default()
    };

    // A case-insensitive substring finds the matching task with board/column
    // names + a snippet; the non-matching task is excluded.
    let out = tool
        .invoke(json!({ "query": "migrat" }), &ctx)
        .await
        .expect("kanban_search_tasks");
    let results = out["results"].as_array().expect("results array");
    assert_eq!(results.len(), 1, "only the matching task");
    assert_eq!(results[0]["title"], json!("Migrate the database"));
    assert_eq!(results[0]["board"], json!("Sprint"));
    assert!(
        results[0].get("snippet").is_some(),
        "carries a body snippet"
    );

    // A term in no task → empty; a blank query is rejected.
    assert!(tool
        .invoke(json!({ "query": "zzznotpresent" }), &ctx)
        .await
        .expect("empty")["results"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(tool.invoke(json!({ "query": "  " }), &ctx).await.is_err());
}

#[tokio::test]
async fn search_events_tool_finds_past_events_by_text() {
    let Some(url) = db_url() else {
        eprintln!("skipping search_events test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    use catalerum_core::{EntityId, EntityKind, EntityRef};
    let store = Store::connect(&url).await.expect("store");
    let ws = store
        .workspaces()
        .create("searchev", &format!("searchev-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let cal = store
        .calendars()
        .upsert_local(ws.id, "default", "Calendar")
        .await
        .expect("cal");

    // A long-past event (body carries the searchable text, plus a named
    // attendee), and a much later same-word event to prove ordering + the
    // date bound.
    store
        .events()
        .create(&UpsertEvent {
            workspace_id: ws.id,
            calendar_id: cal.id,
            uid: "past-kickoff",
            starts_at: "2024-11-05T09:00:00Z".parse().unwrap(),
            ends_at: "2024-11-05T10:00:00Z".parse().unwrap(),
            all_day: false,
            rrule: None,
            summary: "Architecture kickoff",
            location: None,
            body: Some("Deciding the data platform with the whole team."),
            attendees: &[EntityRef {
                workspace_id: ws.id,
                entity_id: EntityId::new(),
                kind: EntityKind::Person,
                display_name: Some("Alice Zephyr".to_string()),
            }],
            labels: &[],
            attachments: &[],
            etag: None,
            sequence: 0,
        })
        .await
        .expect("past event");
    store
        .events()
        .create(&UpsertEvent {
            workspace_id: ws.id,
            calendar_id: cal.id,
            uid: "later-kickoff",
            starts_at: "2030-09-01T09:00:00Z".parse().unwrap(),
            ends_at: "2030-09-01T10:00:00Z".parse().unwrap(),
            all_day: false,
            rrule: None,
            summary: "Kickoff party",
            location: Some("Rooftop"),
            body: None,
            attendees: &[],
            labels: &[],
            attachments: &[],
            etag: None,
            sequence: 0,
        })
        .await
        .expect("later event");

    let tool = SearchEventsTool {
        store: store.clone(),
    };
    let ctx = ToolContext {
        workspace_id: Some(ws.id),
        ..Default::default()
    };

    // A summary substring finds BOTH events — past included — most recent
    // start first.
    let out = tool
        .invoke(json!({ "query": "kickoff" }), &ctx)
        .await
        .expect("search_events");
    let results = out["results"].as_array().expect("results array");
    assert_eq!(results.len(), 2, "past and future both match: {results:?}");
    assert_eq!(results[0]["summary"], json!("Kickoff party"));
    assert_eq!(results[1]["summary"], json!("Architecture kickoff"));

    // A `to` bound narrows to the past event only.
    let past_only = tool
        .invoke(
            json!({ "query": "kickoff", "to": "2025-01-01T00:00:00Z" }),
            &ctx,
        )
        .await
        .expect("bounded search");
    let results = past_only["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["summary"], json!("Architecture kickoff"));

    // Body text matches too, and the hit carries a match-centred snippet.
    let by_body = tool
        .invoke(json!({ "query": "data platform" }), &ctx)
        .await
        .expect("body search");
    let results = by_body["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert!(
        results[0]["snippet"]
            .as_str()
            .is_some_and(|s| s.contains("data platform")),
        "carries a body snippet: {results:?}"
    );

    // An attendee's display name matches; the raw JSON keys must not (a
    // query like "display_name" hits no event).
    let by_attendee = tool
        .invoke(json!({ "query": "alice" }), &ctx)
        .await
        .expect("attendee search");
    assert_eq!(by_attendee["results"].as_array().unwrap().len(), 1);
    assert!(tool
        .invoke(json!({ "query": "display_name" }), &ctx)
        .await
        .expect("key query")["results"]
        .as_array()
        .unwrap()
        .is_empty());

    // A blank query and an inverted window are rejected.
    assert!(tool.invoke(json!({ "query": "  " }), &ctx).await.is_err());
    assert!(tool
        .invoke(
            json!({ "query": "kickoff", "from": "2026-01-01T00:00:00Z", "to": "2025-01-01T00:00:00Z" }),
            &ctx,
        )
        .await
        .is_err());
}

#[tokio::test]
async fn delete_event_tool_removes_a_local_event() {
    let Some(url) = db_url() else {
        eprintln!("skipping delete_event test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    let store = Store::connect(&url).await.expect("store");
    let ws = store
        .workspaces()
        .create("delev", &format!("delev-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let cal = store
        .calendars()
        .upsert_local(ws.id, "default", "Calendar")
        .await
        .unwrap();
    let event = store
        .events()
        .create(&UpsertEvent {
            workspace_id: ws.id,
            calendar_id: cal.id,
            uid: "del",
            starts_at: "2026-05-01T09:00:00Z".parse().unwrap(),
            ends_at: "2026-05-01T10:00:00Z".parse().unwrap(),
            all_day: false,
            rrule: None,
            summary: "Doomed",
            location: None,
            body: None,
            attendees: &[],
            labels: &[],
            attachments: &[],
            etag: None,
            sequence: 0,
        })
        .await
        .unwrap();

    let tool = DeleteEventTool {
        store: store.clone(),
        ingest: NoteIngest::new(store.clone(), false, false),
        secrets: None,
    };
    let ctx = ToolContext {
        workspace_id: Some(ws.id),
        ..Default::default()
    };

    // Deleting the event removes it; a second delete (now missing) errors.
    let out = tool
        .invoke(json!({ "event_id": event.id }), &ctx)
        .await
        .unwrap();
    assert_eq!(out["deleted"], json!(event.id));
    assert!(
        store.events().get(ws.id, event.id).await.is_err(),
        "the event is gone after delete"
    );
    assert!(
        tool.invoke(json!({ "event_id": event.id }), &ctx)
            .await
            .is_err(),
        "deleting a missing event errors"
    );
    // A missing workspace context is rejected.
    assert!(tool
        .invoke(json!({ "event_id": event.id }), &ToolContext::default())
        .await
        .is_err());
}

/// `create_calendar` is get-or-create by name: it returns a **local** calendar
/// with the id `create_event` / a `WriteEvent` action can target, and asking for
/// the same name again returns the same calendar (no duplicate) while a
/// different name yields a distinct one.
#[tokio::test]
async fn create_calendar_tool_is_idempotent_by_name() {
    let Some(url) = db_url() else {
        eprintln!("skipping create_calendar test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    let store = Store::connect(&url).await.expect("store");
    let ws = store
        .workspaces()
        .create("mkcal", &format!("mkcal-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let tool = CreateCalendarTool {
        store: store.clone(),
    };
    let ctx = ToolContext {
        workspace_id: Some(ws.id),
        ..Default::default()
    };

    let first = tool
        .invoke(json!({ "name": "  Planning  " }), &ctx)
        .await
        .expect("create");
    assert_eq!(first["name"], json!("Planning"), "name is trimmed");
    assert!(
        first["connection_id"].is_null(),
        "a created calendar is local (no provider connection)"
    );
    let id: CalendarId = serde_json::from_value(first["id"].clone()).unwrap();
    assert!(store.calendars().get(ws.id, id).await.unwrap().is_local());

    // A second create with the same name (differing only in case/whitespace)
    // returns the SAME calendar — get-or-create, not a duplicate.
    let second = tool
        .invoke(json!({ "name": "planning" }), &ctx)
        .await
        .expect("create again");
    assert_eq!(first["id"], second["id"], "same name, one calendar");

    // A different name is a distinct calendar.
    let other = tool
        .invoke(json!({ "name": "Travel" }), &ctx)
        .await
        .expect("create other");
    assert_ne!(
        first["id"], other["id"],
        "distinct names, distinct calendars"
    );

    // Exactly two local calendars now exist for the two distinct names (the
    // repeated "Planning" did not add a third).
    let all = store.calendars().list_by_workspace(ws.id).await.unwrap();
    assert_eq!(
        all.iter().filter(|c| c.is_local()).count(),
        2,
        "no duplicate calendars from the repeated same-name create"
    );

    // A blank name and a missing workspace context are rejected.
    assert!(tool.invoke(json!({ "name": "  " }), &ctx).await.is_err());
    assert!(tool
        .invoke(json!({ "name": "X" }), &ToolContext::default())
        .await
        .is_err());
}

/// `current_time` resolves its timezone argument > profile > UTC, always
/// returns a UTC anchor + unix seconds, and rejects an unknown IANA name.
#[tokio::test]
async fn current_time_tool_resolves_timezone_by_precedence() {
    let Some(url) = db_url() else {
        eprintln!("skipping current_time test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    use catalerum_core::UserId;

    let store = Store::connect(&url).await.expect("store");
    let ws = store
        .workspaces()
        .create("clock", &format!("clock-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let user = UserId::new();
    let tool = CurrentTimeTool {
        profiles: store.profiles(),
    };
    let ctx = ToolContext {
        workspace_id: Some(ws.id),
        user_id: Some(user),
        ..Default::default()
    };

    // Default path: no profile timezone set, no argument → UTC.
    let out = tool.invoke(json!({}), &ctx).await.expect("now");
    assert_eq!(out["timezone"], json!("UTC"));
    assert_eq!(out["timezone_source"], json!("default"));
    assert_eq!(out["utc_offset"], json!("+00:00"));
    // The UTC anchor is always present, RFC3339 `Z`, and plausibly recent
    // (after 2023-01-01) — proves it's the real clock, not a fixed stub.
    assert!(out["utc"].as_str().unwrap().ends_with('Z'));
    assert!(out["unix"].as_i64().unwrap() > 1_672_531_200);

    // Argument path: an explicit IANA name wins and is echoed back, with a
    // non-UTC offset rendered for the local time.
    let berlin = tool
        .invoke(json!({ "timezone": "Europe/Berlin" }), &ctx)
        .await
        .expect("berlin");
    assert_eq!(berlin["timezone"], json!("Europe/Berlin"));
    assert_eq!(berlin["timezone_source"], json!("argument"));
    assert_ne!(
        berlin["utc_offset"],
        json!("+00:00"),
        "Berlin is offset from UTC"
    );

    // Profile path: with a stored profile timezone and no argument, it is used.
    store
        .profiles()
        .merge(
            ws.id,
            user,
            &[("timezone".to_string(), json!("America/New_York"))]
                .into_iter()
                .collect(),
        )
        .await
        .expect("set tz");
    let ny = tool.invoke(json!({}), &ctx).await.expect("ny");
    assert_eq!(ny["timezone"], json!("America/New_York"));
    assert_eq!(ny["timezone_source"], json!("profile"));

    // An explicit argument still overrides the stored profile timezone.
    let override_utc = tool
        .invoke(json!({ "timezone": "UTC" }), &ctx)
        .await
        .expect("override");
    assert_eq!(override_utc["timezone_source"], json!("argument"));

    // An unknown timezone argument is a hard error; a garbage *profile*
    // value instead silently falls back to the default (UTC).
    assert!(tool
        .invoke(json!({ "timezone": "Mars/Olympus_Mons" }), &ctx)
        .await
        .is_err());
    store
        .profiles()
        .merge(
            ws.id,
            user,
            &[("timezone".to_string(), json!("not-a-zone"))]
                .into_iter()
                .collect(),
        )
        .await
        .expect("bad tz");
    let fallback = tool.invoke(json!({}), &ctx).await.expect("fallback");
    assert_eq!(fallback["timezone"], json!("UTC"));
    assert_eq!(fallback["timezone_source"], json!("default"));

    // Works with no acting user/workspace at all (pure utility) → UTC.
    let bare = tool
        .invoke(json!({}), &ToolContext::default())
        .await
        .expect("bare");
    assert_eq!(bare["timezone"], json!("UTC"));
}

#[tokio::test]
async fn delete_note_tool_removes_a_note_and_reconciles() {
    let Some(url) = db_url() else {
        eprintln!("skipping delete_note test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    use catalerum_core::model::Author;
    use catalerum_core::UserId;
    let store = Store::connect(&url).await.expect("store");
    let ws = store
        .workspaces()
        .create("delnote", &format!("delnote-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let author = Author::User { id: UserId::new() };
    let note = store
        .notes()
        .create(ws.id, author, "Scratch", "tmp", &[])
        .await
        .unwrap();

    // embed/graph off → the reconcile enqueue is a no-op, but still exercised.
    let tool = DeleteNoteTool {
        notes: store.notes(),
        ingest: NoteIngest::new(store.clone(), false, false),
    };
    let ctx = ToolContext {
        workspace_id: Some(ws.id),
        ..Default::default()
    };

    let out = tool.invoke(json!({ "id": note.id }), &ctx).await.unwrap();
    assert_eq!(out["deleted"], json!(note.id));
    assert!(
        !store
            .notes()
            .list_by_workspace(ws.id, catalerum_store::DEFAULT_NOTE_LIMIT)
            .await
            .unwrap()
            .iter()
            .any(|n| n.id == note.id),
        "the note is gone after delete"
    );
    // Deleting a missing note errors; a missing workspace is rejected.
    assert!(tool.invoke(json!({ "id": note.id }), &ctx).await.is_err());
    assert!(tool
        .invoke(json!({ "id": note.id }), &ToolContext::default())
        .await
        .is_err());
}

#[tokio::test]
async fn query_structured_task_operations_carry_board_and_filter_status() {
    let Some(url) = db_url() else {
        eprintln!(
            "skipping query_structured task test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
        );
        return;
    };
    let store = Store::connect(&url).await.expect("store");
    let ws = store
        .workspaces()
        .create("qs", &format!("qs-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    // A default board (Backlog / To-do / Doing / Done columns).
    let board = store.boards().create(ws.id, "Sprint", &[]).await.unwrap();
    let todo = board.columns.iter().find(|c| c.name == "To-do").unwrap().id;
    let doing = board.columns.iter().find(|c| c.name == "Doing").unwrap().id;

    let ship = store
        .tasks()
        .create(ws.id, board.id, todo, "Ship release", "", None)
        .await
        .unwrap();
    let review = store
        .tasks()
        .create(ws.id, board.id, doing, "Review PR", "", None)
        .await
        .unwrap();
    // Move `review` to in_progress and finish `ship`.
    store
        .tasks()
        .set_status(ws.id, review.id, TaskStatus::InProgress)
        .await
        .unwrap();
    store
        .tasks()
        .set_status(ws.id, ship.id, TaskStatus::Done)
        .await
        .unwrap();

    let tool = QueryStructuredTool {
        store: store.clone(),
        storage: None,
    };
    let ctx = ToolContext {
        workspace_id: Some(ws.id),
        ..Default::default()
    };

    // open_tasks excludes the Done task; the in_progress one carries its
    // board + column names.
    let open = tool
        .invoke(json!({ "operation": "open_tasks" }), &ctx)
        .await
        .unwrap();
    let rows = open["results"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "only the not-done task is open");
    assert_eq!(rows[0]["title"], json!("Review PR"));
    assert_eq!(rows[0]["status"], json!("in_progress"));
    assert_eq!(rows[0]["board"], json!("Sprint"));
    assert_eq!(rows[0]["column"], json!("Doing"));

    // tasks_by_status done returns exactly the finished task.
    let done = tool
        .invoke(
            json!({ "operation": "tasks_by_status", "status": "done" }),
            &ctx,
        )
        .await
        .unwrap();
    let done_rows = done["results"].as_array().unwrap();
    assert_eq!(done_rows.len(), 1);
    assert_eq!(done_rows[0]["title"], json!("Ship release"));

    // tasks_by_board returns every task on the named board (any status),
    // matching the name case-insensitively.
    let by_board = tool
        .invoke(
            json!({ "operation": "tasks_by_board", "board": "sprint" }),
            &ctx,
        )
        .await
        .unwrap();
    let board_rows = by_board["results"].as_array().unwrap();
    assert_eq!(
        board_rows.len(),
        2,
        "both tasks on the board, regardless of status"
    );
    assert!(board_rows.iter().all(|r| r["board"] == json!("Sprint")));

    // boards enumerates the workspace's boards with their column ids — what the
    // agent needs to call create_task (board_id + column_id) or to learn valid
    // board names for tasks_by_board.
    let boards = tool
        .invoke(json!({ "operation": "boards" }), &ctx)
        .await
        .unwrap();
    let board_list = boards["results"].as_array().unwrap();
    assert_eq!(board_list.len(), 1);
    assert_eq!(board_list[0]["id"], json!(board.id));
    assert_eq!(board_list[0]["name"], json!("Sprint"));
    let cols = board_list[0]["columns"].as_array().unwrap();
    assert!(cols.iter().any(|c| c["name"] == json!("Doing")));
    assert!(cols.iter().all(|c| c["id"].is_string()));

    // Another workspace sees no tasks (§18).
    let other = ToolContext {
        workspace_id: Some(WorkspaceId::new()),
        ..Default::default()
    };
    let none = tool
        .invoke(json!({ "operation": "open_tasks" }), &other)
        .await
        .unwrap();
    assert!(none["results"].as_array().unwrap().is_empty());

    // Bad inputs: tasks_by_status without a status, and an unknown status.
    assert!(tool
        .invoke(json!({ "operation": "tasks_by_status" }), &ctx)
        .await
        .is_err());
    assert!(tool
        .invoke(
            json!({ "operation": "tasks_by_status", "status": "wat" }),
            &ctx
        )
        .await
        .is_err());
    // tasks_by_board without a board, and an unknown board name, both error.
    assert!(tool
        .invoke(json!({ "operation": "tasks_by_board" }), &ctx)
        .await
        .is_err());
    assert!(tool
        .invoke(
            json!({ "operation": "tasks_by_board", "board": "Nonexistent" }),
            &ctx
        )
        .await
        .is_err());
}

#[tokio::test]
async fn memory_tools_remember_recall_forget_with_user_visibility() {
    let Some(url) = db_url() else {
        eprintln!("skipping memory tools test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    use catalerum_core::UserId;

    let store = Store::connect(&url).await.expect("store");
    let ws = store
        .workspaces()
        .create("memtools", &format!("memtools-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let memories = store.memories();
    // embed=false → the ingest hook is a no-op (no Qdrant needed for this test).
    let ingest = NoteIngest::new(store.clone(), false, false);
    // `search: None` → heuristic-only dedup + no embed-enqueue (no Qdrant needed).
    let remember = RememberTool {
        store: store.clone(),
        search: None,
    };
    let recall = RecallTool {
        memories: memories.clone(),
    };
    let update = UpdateMemoryTool {
        memories: memories.clone(),
        ingest: ingest.clone(),
    };
    let forget = ForgetTool { memories, ingest };

    let alice = UserId::new();
    let ctx_a = ToolContext {
        workspace_id: Some(ws.id),
        user_id: Some(alice),
        ..Default::default()
    };

    // Alice remembers a private fact and a shared one.
    let priv_mem = remember
        .invoke(json!({ "text": "prefers tea", "scope": "user" }), &ctx_a)
        .await
        .unwrap();
    assert_eq!(priv_mem["scope"], json!("user"));
    remember
        .invoke(
            json!({ "text": "office is in Berlin", "scope": "workspace" }),
            &ctx_a,
        )
        .await
        .unwrap();
    // An agent with no user defaults a user-scoped request to workspace.
    let agent_mem = remember
        .invoke(
            json!({ "text": "deploys on Fridays" }),
            &ToolContext {
                workspace_id: Some(ws.id),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(agent_mem["scope"], json!("workspace"));

    // Alice recalls all three (her private + two workspace), most recent first.
    let recalled = recall.invoke(json!({}), &ctx_a).await.unwrap();
    let mems = recalled["memories"].as_array().unwrap();
    assert_eq!(mems.len(), 3);
    assert_eq!(mems[0]["text"], json!("deploys on Fridays"));

    // Dedup seam (SOUL §29): re-remembering the SAME fact (whitespace/case
    // aside) adds no row — it is `deduplicated` and returns the existing one.
    let dup = remember
        .invoke(
            json!({ "text": "  Prefers   TEA ", "scope": "user" }),
            &ctx_a,
        )
        .await
        .unwrap();
    assert_eq!(dup["status"], json!("deduplicated"));
    assert_eq!(dup["id"], priv_mem["id"]);
    assert_eq!(
        recall.invoke(json!({}), &ctx_a).await.unwrap()["memories"]
            .as_array()
            .unwrap()
            .len(),
        3,
        "an exact duplicate must not add a row"
    );
    // A strict extension of a known fact `refine`s it in place (same id, new
    // text) rather than storing a near-duplicate.
    let refined = remember
        .invoke(
            json!({ "text": "prefers tea in the morning", "scope": "user" }),
            &ctx_a,
        )
        .await
        .unwrap();
    assert_eq!(refined["status"], json!("refined"));
    assert_eq!(refined["id"], priv_mem["id"]);
    assert_eq!(refined["text"], json!("prefers tea in the morning"));
    assert_eq!(
        recall.invoke(json!({}), &ctx_a).await.unwrap()["memories"]
            .as_array()
            .unwrap()
            .len(),
        3,
        "a refinement updates in place, never inserts"
    );
    // A genuinely new, unrelated fact is `stored`.
    let stored = remember
        .invoke(
            json!({ "text": "drives a red car", "scope": "user" }),
            &ctx_a,
        )
        .await
        .unwrap();
    assert_eq!(stored["status"], json!("stored"));
    // Clean it back up so the remaining assertions keep counting three.
    forget
        .invoke(json!({ "id": stored["id"].as_str().unwrap() }), &ctx_a)
        .await
        .unwrap();

    // Bob (a different user) sees the two workspace memories, not Alice's tea.
    let ctx_b = ToolContext {
        workspace_id: Some(ws.id),
        user_id: Some(UserId::new()),
        ..Default::default()
    };
    let bob_recall = recall.invoke(json!({}), &ctx_b).await.unwrap();
    let bob_texts: Vec<&str> = bob_recall["memories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["text"].as_str().unwrap())
        .collect();
    assert_eq!(bob_texts.len(), 2);
    assert!(!bob_texts.contains(&"prefers tea"));

    // Alice corrects her private memory's text in place — id + scope kept, no
    // new row (still 3 of hers), and the new text is what recall returns.
    let updated = update
        .invoke(
            json!({ "id": priv_mem["id"].clone(), "text": "prefers black coffee" }),
            &ctx_a,
        )
        .await
        .unwrap();
    assert_eq!(updated["id"], priv_mem["id"]);
    assert_eq!(updated["scope"], json!("user"));
    assert_eq!(updated["text"], json!("prefers black coffee"));
    let after_update = recall.invoke(json!({}), &ctx_a).await.unwrap();
    let after_texts: Vec<&str> = after_update["memories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["text"].as_str().unwrap())
        .collect();
    assert_eq!(after_texts.len(), 3);
    assert!(after_texts.contains(&"prefers black coffee"));
    assert!(!after_texts.contains(&"prefers tea"));

    // Forget Alice's private memory; recall no longer shows it.
    let id = priv_mem["id"].as_str().unwrap();
    let forgotten = forget.invoke(json!({ "id": id }), &ctx_a).await.unwrap();
    assert_eq!(forgotten["forgotten"], json!(true));
    let after = recall.invoke(json!({}), &ctx_a).await.unwrap();
    assert_eq!(after["memories"].as_array().unwrap().len(), 2);

    // Bad inputs: empty text, missing workspace, bad id, forgetting twice.
    assert!(remember
        .invoke(json!({ "text": "  " }), &ctx_a)
        .await
        .is_err());
    assert!(remember
        .invoke(json!({ "text": "x" }), &ToolContext::default())
        .await
        .is_err());
    assert!(forget
        .invoke(json!({ "id": "nope" }), &ctx_a)
        .await
        .is_err());
    assert!(forget.invoke(json!({ "id": id }), &ctx_a).await.is_err());
    // update_memory rejects blank text and an unparseable id.
    assert!(update
        .invoke(json!({ "id": id, "text": "  " }), &ctx_a)
        .await
        .is_err());
    assert!(update
        .invoke(json!({ "id": "nope", "text": "x" }), &ctx_a)
        .await
        .is_err());
}

#[tokio::test]
async fn update_profile_merges_fields_and_requires_a_user() {
    let Some(url) = db_url() else {
        eprintln!("skipping update_profile test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    use catalerum_core::UserId;

    let store = Store::connect(&url).await.expect("store");
    let ws = store
        .workspaces()
        .create("prof", &format!("prof-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let user = UserId::new();
    let tool = UpdateProfileTool {
        profiles: store.profiles(),
    };
    let ctx = ToolContext {
        workspace_id: Some(ws.id),
        user_id: Some(user),
        ..Default::default()
    };

    // First write sets a field.
    let p = tool
        .invoke(json!({ "fields": { "timezone": "Europe/Berlin" } }), &ctx)
        .await
        .unwrap();
    assert_eq!(p["fields"]["timezone"], json!("Europe/Berlin"));

    // Second write merges (preserves timezone, adds focus_hours).
    let p2 = tool
        .invoke(json!({ "fields": { "focus_hours": 4 } }), &ctx)
        .await
        .unwrap();
    assert_eq!(p2["fields"]["timezone"], json!("Europe/Berlin"));
    assert_eq!(p2["fields"]["focus_hours"], json!(4));

    // An overlapping key overrides; the stored profile reflects it.
    tool.invoke(json!({ "fields": { "timezone": "UTC" } }), &ctx)
        .await
        .unwrap();
    let stored = store.profiles().get(ws.id, user).await.unwrap();
    assert_eq!(stored.fields.get("timezone"), Some(&json!("UTC")));
    assert_eq!(stored.fields.get("focus_hours"), Some(&json!(4)));

    // Bad inputs: no acting user, missing/empty/non-object fields.
    let no_user = ToolContext {
        workspace_id: Some(ws.id),
        ..Default::default()
    };
    assert!(tool
        .invoke(json!({ "fields": { "a": 1 } }), &no_user)
        .await
        .is_err());
    assert!(tool.invoke(json!({}), &ctx).await.is_err());
    assert!(tool.invoke(json!({ "fields": {} }), &ctx).await.is_err());
    assert!(tool
        .invoke(json!({ "fields": "nope" }), &ctx)
        .await
        .is_err());
}

#[tokio::test]
async fn search_semantic_does_not_leak_another_users_private_memory() {
    let (Some(qurl), Some(db)) = (qdrant_url(), db_url()) else {
        eprintln!("skipping memory-leak test: set QDRANT_URL and CATALERUM_TEST_DATABASE_URL");
        return;
    };
    use catalerum_core::model::{Author, MemoryScope};
    use catalerum_core::UserId;
    use catalerum_vector::{PointPayload, VectorPoint};

    let vector = VectorStore::new(&qurl).expect("qdrant");
    let store = Store::connect(&db).await.expect("store");
    let ws = store
        .workspaces()
        .create("leak", &format!("leak-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let _ = vector.delete_collection(ws.id).await;
    let alice = UserId::new();
    let bob = UserId::new();

    // Bob's private memory + a shared workspace memory + a note — all embedded.
    let bob_mem = store
        .memories()
        .create(
            ws.id,
            MemoryScope::User,
            Some(bob),
            "bob's secret api key",
            None,
        )
        .await
        .unwrap();
    let shared = store
        .memories()
        .create(
            ws.id,
            MemoryScope::Workspace,
            None,
            "office wifi password",
            None,
        )
        .await
        .unwrap();
    let note = store
        .notes()
        .create(ws.id, Author::User { id: alice }, "Roadmap", "themes", &[])
        .await
        .unwrap();

    vector.ensure_collection(ws.id, DIM).await.unwrap();
    vector
        .upsert(
            ws.id,
            &[
                VectorPoint::new(
                    fake_vec("bob's secret api key"),
                    PointPayload::new(
                        ws.id,
                        SourceRef::Memory { id: bob_mem.id },
                        "bob's secret api key",
                    ),
                ),
                VectorPoint::new(
                    fake_vec("office wifi password"),
                    PointPayload::new(
                        ws.id,
                        SourceRef::Memory { id: shared.id },
                        "office wifi password",
                    ),
                ),
                VectorPoint::new(
                    fake_vec("themes"),
                    PointPayload::new(ws.id, SourceRef::Note { id: note.id }, "themes"),
                ),
            ],
        )
        .await
        .unwrap();

    let tool = SearchSemanticTool {
        search: SemanticSearch {
            embedder: Arc::new(FakeEmbedder),
            vector: vector.clone(),
            embed_model: "fake".into(),
        },
        store: store.clone(),
    };
    let texts = |out: &Json| -> Vec<String> {
        out["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["text"].as_str().unwrap().to_string())
            .collect()
    };

    // Alice sees the shared memory + the note, but NOT Bob's private memory.
    let ctx_a = ToolContext {
        workspace_id: Some(ws.id),
        user_id: Some(alice),
        ..Default::default()
    };
    let a = tool
        .invoke(json!({ "query": "anything", "limit": 10 }), &ctx_a)
        .await
        .unwrap();
    let a_texts = texts(&a);
    assert!(a_texts.contains(&"office wifi password".to_string()));
    assert!(a_texts.contains(&"themes".to_string()));
    assert!(
        !a_texts.contains(&"bob's secret api key".to_string()),
        "private memory leaked to Alice!"
    );

    // Bob sees his own private memory.
    let ctx_b = ToolContext {
        workspace_id: Some(ws.id),
        user_id: Some(bob),
        ..Default::default()
    };
    let b = tool
        .invoke(json!({ "query": "anything", "limit": 10 }), &ctx_b)
        .await
        .unwrap();
    assert!(texts(&b).contains(&"bob's secret api key".to_string()));

    // An agent run (no user) sees neither private memory.
    let ctx_agent = ToolContext {
        workspace_id: Some(ws.id),
        ..Default::default()
    };
    let ag = tool
        .invoke(json!({ "query": "anything", "limit": 10 }), &ctx_agent)
        .await
        .unwrap();
    assert!(!texts(&ag).contains(&"bob's secret api key".to_string()));
    assert!(texts(&ag).contains(&"office wifi password".to_string()));

    let _ = vector.delete_collection(ws.id).await;
}

#[tokio::test]
async fn recall_memory_texts_is_visibility_filtered_deduped_and_limited() {
    let (Some(qurl), Some(db)) = (qdrant_url(), db_url()) else {
        eprintln!(
            "skipping recall_memory_texts test: set QDRANT_URL and CATALERUM_TEST_DATABASE_URL"
        );
        return;
    };
    use catalerum_core::model::MemoryScope;
    use catalerum_core::UserId;
    use catalerum_vector::{PointPayload, VectorPoint};

    let vector = VectorStore::new(&qurl).expect("qdrant");
    let store = Store::connect(&db).await.expect("store");
    let ws = store
        .workspaces()
        .create("recall", &format!("recall-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let _ = vector.delete_collection(ws.id).await;
    let alice = UserId::new();
    let bob = UserId::new();

    let a_mem = store
        .memories()
        .create(
            ws.id,
            MemoryScope::User,
            Some(alice),
            "alice likes tea",
            None,
        )
        .await
        .unwrap();
    let b_mem = store
        .memories()
        .create(ws.id, MemoryScope::User, Some(bob), "bob's pin code", None)
        .await
        .unwrap();
    let shared = store
        .memories()
        .create(
            ws.id,
            MemoryScope::Workspace,
            None,
            "team standup at 9",
            None,
        )
        .await
        .unwrap();

    vector.ensure_collection(ws.id, DIM).await.unwrap();
    let seed = |id, text: &str| {
        VectorPoint::new(
            fake_vec(text),
            PointPayload::new(ws.id, SourceRef::Memory { id }, text),
        )
    };
    vector
        .upsert(
            ws.id,
            &[
                seed(a_mem.id, "alice likes tea"),
                seed(b_mem.id, "bob's pin code"),
                seed(shared.id, "team standup at 9"),
            ],
        )
        .await
        .unwrap();

    let search = SemanticSearch {
        embedder: Arc::new(FakeEmbedder),
        vector: vector.clone(),
        embed_model: "fake".into(),
    };

    // Alice recalls her own + the workspace memory, never Bob's private one.
    let got = recall_memory_texts(&store, &search, ws.id, Some(alice), "alice likes tea", 5).await;
    assert!(got.contains(&"alice likes tea".to_string()));
    assert!(got.contains(&"team standup at 9".to_string()));
    assert!(
        !got.contains(&"bob's pin code".to_string()),
        "private memory recalled across users!"
    );

    // The query exactly matches Alice's memory, so it ranks first.
    assert_eq!(got.first().map(String::as_str), Some("alice likes tea"));

    // limit is respected.
    let one = recall_memory_texts(&store, &search, ws.id, Some(alice), "alice likes tea", 1).await;
    assert_eq!(one.len(), 1);

    // Empty/blank query and the no-backend path yield nothing.
    assert!(
        recall_memory_texts(&store, &search, ws.id, Some(alice), "   ", 5)
            .await
            .is_empty()
    );

    let _ = vector.delete_collection(ws.id).await;
}

#[tokio::test]
async fn every_registered_tool_capability_domain_is_role_grantable() {
    // Regression guard for the `web:read` bug class (SOUL §19/§27): a tool gated
    // on a capability whose **domain is not in any base role's set** — and that
    // isn't a known protected grant-scope — is silently denied for every
    // non-Owner/Admin caller (Owner/Admin hold the `*` wildcard, so the gap hides
    // behind them). `fetch_url` had exactly this shape until `web` was registered.
    // So: every registered tool's required-capability domain must be holdable by a
    // **non-wildcard** base role (Viewer/Member) OR be an intentional protected
    // scope (host `exec`, MCP `expose`) that requires an explicit grant.
    let Some(db) = db_url() else {
        eprintln!("skipping tool-capability-domain guard: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    use catalerum_core::model::Role;
    let store = Store::connect(&db).await.expect("store");

    // A no-op fetcher so the egress tool (`fetch_url`) registers and is checked;
    // the guard only inspects tool metadata, so `fetch` is never invoked.
    struct StubFetcher;
    #[async_trait]
    impl WebFetcher for StubFetcher {
        async fn fetch(
            &self,
            _req: catalerum_core::provider::FetchRequest,
        ) -> catalerum_core::error::Result<catalerum_core::provider::FetchedPage> {
            unreachable!("the capability-domain guard never fetches")
        }
    }
    let fetcher: Arc<dyn WebFetcher> = Arc::new(StubFetcher);
    let registry = build_registry(
        &store,
        Some(&fetcher),
        NoteIngest::new(store.clone(), false, false),
        None,
        None,
        None,
        Vec::new(),
        None,
        None,
    );

    // Domains that are intentionally NOT role-base — they require an explicit
    // §19 grant or admin authority, so a base role never holding them is correct:
    // host command exec, MCP server expose, and agent-profile management (admin-only,
    // like grants — only an Owner/Admin `*` covers `agent_profile:*`; SOUL §19/§25).
    const PROTECTED: &[&str] = &["exec", "mcp", "agent_profile"];

    let names: Vec<String> = registry.names().map(str::to_owned).collect();
    assert!(!names.is_empty(), "the registry should not be empty");
    for name in &names {
        let tool = registry.get(name).expect("registered tool");
        let Some(req) = tool.required_capability() else {
            continue; // ungated tool — nothing to check
        };
        let domain = req.resource.domain.as_str();
        // Is the *domain* a registered base domain? Probe a base READ on it via the
        // non-wildcard Member role — independent of the tool's own verb, which may
        // legitimately be the protected Delete (delete/exec/expose are never
        // role-base; SOUL §19). An unregistered domain (the `web:read` bug) fails
        // this regardless of verb.
        let domain_is_registered = catalerum_iam::role_allows(
            Role::Member,
            &Capability::new(Action::Read, Resource::domain(domain)),
        );
        assert!(
            domain_is_registered || PROTECTED.contains(&domain),
            "tool `{name}` uses the capability domain `{domain}`, which isn't a \
             registered base domain in catalerum-iam's DOMAINS and isn't a known \
             protected grant-scope — so no role below Owner/Admin can ever hold it \
             and the tool is silently denied for every non-admin caller (the \
             `web:read` bug class). Register `{domain}` in DOMAINS, or add it to \
             PROTECTED if it is a grant-only scope."
        );
    }
}

#[tokio::test]
async fn tool_dispatch_enforces_role_capabilities() {
    let Some(db) = db_url() else {
        eprintln!("skipping role-capability test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    use catalerum_core::model::Role;
    use catalerum_core::UserId;

    let store = Store::connect(&db).await.expect("store");
    let ws = store
        .workspaces()
        .create("caps", &format!("caps-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let registry = build_registry(
        &store,
        None,
        NoteIngest::new(store.clone(), false, false),
        None,
        None,
        None,
        Vec::new(),
        None,
        None,
    );
    let user = UserId::new();
    let ctx = |role: Role| ToolContext {
        workspace_id: Some(ws.id),
        user_id: Some(user),
        capabilities: Some(catalerum_iam::base_capabilities(role)),
        ..Default::default()
    };
    let title = json!({ "title": "x" });

    // A Viewer may read/recall but not write notes or memories (deny-by-default).
    assert!(matches!(
        registry
            .dispatch("create_note", title.clone(), &ctx(Role::Viewer))
            .await,
        Err(Error::Unauthorized(_))
    ));
    assert!(matches!(
        registry
            .dispatch("remember", json!({ "text": "x" }), &ctx(Role::Viewer))
            .await,
        Err(Error::Unauthorized(_))
    ));
    // Reads are allowed for a Viewer.
    assert!(registry
        .dispatch("list_notes", json!({}), &ctx(Role::Viewer))
        .await
        .is_ok());
    assert!(registry
        .dispatch("recall", json!({}), &ctx(Role::Viewer))
        .await
        .is_ok());

    // A Member may write.
    assert!(registry
        .dispatch("create_note", title.clone(), &ctx(Role::Member))
        .await
        .is_ok());

    // An Owner (wildcard) may do anything.
    assert!(registry
        .dispatch("create_note", title, &ctx(Role::Owner))
        .await
        .is_ok());

    // No capabilities supplied → enforcement off (legacy path still works).
    let no_caps = ToolContext {
        workspace_id: Some(ws.id),
        user_id: Some(user),
        ..Default::default()
    };
    assert!(registry
        .dispatch("create_note", json!({ "title": "y" }), &no_caps)
        .await
        .is_ok());
}

#[tokio::test]
async fn edit_ui_applies_partial_patch_and_revalidates() {
    let Some(db) = db_url() else {
        eprintln!("skipping edit_ui test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    use catalerum_core::model::Role;
    use catalerum_core::{UiSpec, UserId};

    let store = Store::connect(&db).await.expect("store");
    let ws = store
        .workspaces()
        .create("editui", &format!("editui-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let registry = build_registry(
        &store,
        None,
        NoteIngest::new(store.clone(), false, false),
        None,
        None,
        None,
        Vec::new(),
        None,
        None,
    );
    let ctx = ToolContext {
        workspace_id: Some(ws.id),
        user_id: Some(UserId::new()),
        capabilities: Some(catalerum_iam::base_capabilities(Role::Member)),
        ..Default::default()
    };

    // Author a small UI with present_ui.
    let created = registry
        .dispatch(
            "present_ui",
            json!({
                "title": "Greeter",
                "name": "greeter",
                "definition": {
                    "default_view": "main",
                    "views": [{ "id": "main", "title": "Main", "root": {
                        "id": "root", "kind": "stack", "children": [
                            { "id": "hello", "kind": "text", "props": { "text": "hi", "tone": "warm" } }
                        ]
                    }}]
                }
            }),
            &ctx,
        )
        .await
        .expect("present_ui");
    let ui_id = created["ui_id"].as_str().expect("ui_id").to_string();
    assert_eq!(created["version"].as_i64(), Some(1));

    // Partially edit: merge one prop + insert a sibling — no full-tree resend.
    let edited = registry
        .dispatch(
            "edit_ui",
            json!({
                "id": ui_id,
                "patch": [
                    { "op": "set_props", "node_id": "hello",
                      "props": { "text": "hello world" }, "merge": true },
                    { "op": "insert_node", "parent_id": "root",
                      "node": { "id": "sub", "kind": "text", "props": { "text": "sub" } } }
                ]
            }),
            &ctx,
        )
        .await
        .expect("edit_ui");
    assert_eq!(edited["ui_id"].as_str(), Some(ui_id.as_str()));
    assert_eq!(
        edited["version"].as_i64(),
        Some(2),
        "an edit bumps the version"
    );

    // Read back: the patch landed, the merge kept the untouched prop, and the
    // rest of the spec is intact.
    let read = registry
        .dispatch("read_ui", json!({ "id": ui_id }), &ctx)
        .await
        .expect("read_ui");
    let spec: UiSpec = serde_json::from_value(read["definition"].clone()).expect("spec");
    let root = &spec.views[0].root;
    assert_eq!(root.children.len(), 2);
    let hello = root
        .children
        .iter()
        .find(|n| n.id == "hello")
        .expect("hello");
    assert_eq!(
        hello.props.get("text").and_then(|v| v.as_str()),
        Some("hello world")
    );
    assert_eq!(
        hello.props.get("tone").and_then(|v| v.as_str()),
        Some("warm")
    );
    assert!(root.children.iter().any(|n| n.id == "sub"));
    assert_eq!(spec.views[0].title, "Main");

    // A patch whose RESULT would be invalid (duplicate node id) is rejected and
    // never persisted.
    let bad = registry
        .dispatch(
            "edit_ui",
            json!({
                "id": ui_id,
                "patch": [ { "op": "insert_node", "parent_id": "root",
                             "node": { "id": "hello", "kind": "text" } } ]
            }),
            &ctx,
        )
        .await;
    assert!(bad.is_err(), "a duplicate-id patch must be rejected");
    let read2 = registry
        .dispatch("read_ui", json!({ "id": ui_id }), &ctx)
        .await
        .expect("read_ui");
    assert_eq!(
        read2["version"].as_i64(),
        Some(2),
        "the rejected edit left the stored version unchanged"
    );
}

#[tokio::test]
async fn create_ui_components_builds_a_staged_app_atomically() {
    let Some(db) = db_url() else {
        eprintln!(
            "skipping staged UI components test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
        );
        return;
    };
    use catalerum_core::model::Role;
    use catalerum_core::{UiSpec, UserId};

    let store = Store::connect(&db).await.expect("store");
    let ws = store
        .workspaces()
        .create("stagedui", &format!("stagedui-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let registry = build_registry(
        &store,
        None,
        NoteIngest::new(store.clone(), false, false),
        None,
        None,
        None,
        Vec::new(),
        None,
        None,
    );
    let ctx = ToolContext {
        workspace_id: Some(ws.id),
        user_id: Some(UserId::new()),
        capabilities: Some(catalerum_iam::base_capabilities(Role::Member)),
        ..Default::default()
    };

    // Create only the tiny shell: no full UiSpec payload is needed.
    let created = registry
        .dispatch(
            "present_ui",
            json!({ "title": "Staged", "name": "staged" }),
            &ctx,
        )
        .await
        .expect("starter app");
    let ui_id = created["ui_id"].as_str().expect("ui_id").to_string();
    assert_eq!(created["root_id"].as_str(), Some("root"));
    assert_eq!(created["default_view"].as_str(), Some("main"));
    assert_eq!(created["next_call_target"]["id"].as_str(), Some(&*ui_id));
    assert!(created["advertise_tools"]
        .as_array()
        .is_some_and(|tools| tools.iter().any(|tool| tool == "edit_ui")));

    // Regression: a component's parent_id is not the App target. Runtime
    // validation preserves this invariant without a provider-incompatible
    // root-level JSON-Schema union.
    let missing_target = registry
        .dispatch(
            "create_ui_components",
            json!({
                "components": [{
                    "parent_id": "root",
                    "node": { "id": "orphan", "kind": "text" }
                }]
            }),
            &ctx,
        )
        .await
        .expect_err("missing App target");
    let missing_target = missing_target.to_string();
    assert!(missing_target.contains("top-level `id`"));
    assert!(missing_target.contains("Do not retry unchanged"));

    // Entries apply in order: the second can target the first without sending
    // either the surrounding App or an edit_ui patch array.
    let components = registry
        .dispatch(
            "create_ui_components",
            json!({
                "name": "staged",
                "components": [
                    { "parent_id": "root", "node": {
                        "id": "profile", "kind": "card", "props": { "title": "Profile" }
                    }},
                    { "parent_id": "profile", "node": {
                        "id": "greeting", "kind": "text", "props": { "text": "Hello" }
                    }}
                ]
            }),
            &ctx,
        )
        .await
        .expect("create components");
    assert_eq!(components["version"].as_i64(), Some(2));
    assert_eq!(components["created"], json!(["profile", "greeting"]));

    // Seed state can be added incrementally after creating the shell; components
    // may then bind/iterate over it without re-sending the whole UiSpec.
    let seeded = registry
        .dispatch(
            "edit_ui",
            json!({
                "name": "staged",
                "patch": [{
                    "op": "set_initial_state",
                    "state": { "recipes": [{ "id": "pad-thai", "title": "Pad Thai" }] },
                    "merge": true
                }]
            }),
            &ctx,
        )
        .await
        .expect("seed initial state");
    assert_eq!(seeded["version"].as_i64(), Some(3));

    let read = registry
        .dispatch("read_ui", json!({ "id": ui_id }), &ctx)
        .await
        .expect("read staged app");
    let spec: UiSpec = serde_json::from_value(read["definition"].clone()).expect("spec");
    let profile = &spec.views[0].root.children[0];
    assert_eq!(profile.id, "profile");
    assert_eq!(profile.children[0].id, "greeting");
    assert_eq!(spec.initial_state["recipes"][0]["id"], "pad-thai");

    // A bad batch is all-or-nothing: the valid first insertion is discarded
    // when the second would duplicate an id in the final tree.
    let bad = registry
        .dispatch(
            "create_ui_components",
            json!({
                "id": ui_id,
                "components": [
                    { "parent_id": "root", "node": { "id": "temporary", "kind": "text" } },
                    { "parent_id": "root", "node": { "id": "greeting", "kind": "text" } }
                ]
            }),
            &ctx,
        )
        .await;
    assert!(bad.is_err(), "invalid component batch must be rejected");
    let after_bad = registry
        .dispatch("read_ui", json!({ "id": ui_id }), &ctx)
        .await
        .expect("read after rejected batch");
    assert_eq!(after_bad["version"].as_i64(), Some(3));
    assert!(
        serde_json::to_string(&after_bad["definition"])
            .expect("definition json")
            .find("temporary")
            .is_none(),
        "the earlier insertion in a rejected batch was not persisted"
    );

    // Replace just one logical subtree, not the surrounding App. The target is
    // a view root or any descendant and the replacement may keep the same id.
    let edited = registry
        .dispatch(
            "edit_ui_components",
            json!({
                "name": "staged",
                "components": [{
                    "node_id": "profile",
                    "node": {
                        "id": "profile", "kind": "card",
                        "props": { "title": "Profile edited" },
                        "children": [{
                            "id": "greeting", "kind": "text",
                            "props": { "text": "Hello again" }
                        }]
                    }
                }]
            }),
            &ctx,
        )
        .await
        .expect("edit component");
    assert_eq!(edited["version"].as_i64(), Some(4));
    assert_eq!(edited["edited"], json!(["profile"]));
    let edited_profile = registry
        .dispatch(
            "read_ui",
            json!({ "id": ui_id, "node_id": "profile" }),
            &ctx,
        )
        .await
        .expect("read edited component");
    assert_eq!(
        edited_profile["node"]["props"]["title"].as_str(),
        Some("Profile edited")
    );
    assert_eq!(
        edited_profile["node"]["children"][0]["props"]["text"].as_str(),
        Some("Hello again")
    );

    // Component edits are atomic too: the first replacement is discarded when
    // a later target in the same call does not exist.
    let bad_edit = registry
        .dispatch(
            "edit_ui_components",
            json!({
                "id": ui_id,
                "components": [
                    { "node_id": "profile", "node": {
                        "id": "profile", "kind": "card",
                        "props": { "title": "Must roll back" }
                    }},
                    { "node_id": "missing", "node": {
                        "id": "missing", "kind": "text"
                    }}
                ]
            }),
            &ctx,
        )
        .await;
    assert!(
        bad_edit.is_err(),
        "invalid component edits must be rejected"
    );
    let after_bad_edit = registry
        .dispatch(
            "read_ui",
            json!({ "id": ui_id, "node_id": "profile" }),
            &ctx,
        )
        .await
        .expect("read after rejected edit");
    assert_eq!(after_bad_edit["version"].as_i64(), Some(4));
    assert_eq!(
        after_bad_edit["node"]["props"]["title"].as_str(),
        Some("Profile edited")
    );

    // Re-presenting a named staged App without `definition` updates metadata
    // and preserves the assembled component tree instead of resetting it.
    registry
        .dispatch(
            "present_ui",
            json!({ "title": "Staged renamed", "name": "staged" }),
            &ctx,
        )
        .await
        .expect("metadata-only present");
    let final_read = registry
        .dispatch("read_ui", json!({ "id": ui_id }), &ctx)
        .await
        .expect("final read");
    assert_eq!(final_read["title"].as_str(), Some("Staged renamed"));
    assert_eq!(
        final_read["definition"]["views"][0]["root"]["children"][0]["id"].as_str(),
        Some("profile")
    );
}

#[tokio::test]
async fn read_ui_scoped_reads_and_replace_node() {
    let Some(db) = db_url() else {
        eprintln!("skipping read_ui scoped test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    use catalerum_core::model::Role;
    use catalerum_core::UserId;

    let store = Store::connect(&db).await.expect("store");
    let ws = store
        .workspaces()
        .create("scopedui", &format!("scopedui-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let registry = build_registry(
        &store,
        None,
        NoteIngest::new(store.clone(), false, false),
        None,
        None,
        None,
        Vec::new(),
        None,
        None,
    );
    let ctx = ToolContext {
        workspace_id: Some(ws.id),
        user_id: Some(UserId::new()),
        capabilities: Some(catalerum_iam::base_capabilities(Role::Member)),
        ..Default::default()
    };

    // Two views: "main" embeds the "frag" view via view_ref — the
    // views-as-component-files layout the scoped reads exist for.
    let created = registry
        .dispatch(
            "present_ui",
            json!({
                "title": "Split",
                "name": "split",
                "definition": {
                    "default_view": "main",
                    "views": [
                        { "id": "main", "title": "Main", "root": {
                            "id": "root", "kind": "stack", "children": [
                                { "id": "head", "kind": "heading", "props": { "level": 2, "text": "Hi" } },
                                { "id": "embed", "kind": "view_ref", "props": { "view": "frag" } }
                            ]
                        }},
                        { "id": "frag", "title": "Fragment", "root": {
                            "id": "frag_root", "kind": "card", "children": [
                                { "id": "frag_txt", "kind": "text", "props": { "text": "old" } }
                            ]
                        }}
                    ]
                }
            }),
            &ctx,
        )
        .await
        .expect("present_ui");
    let ui_id = created["ui_id"].as_str().expect("ui_id").to_string();

    // Outline: skeleton only (ids/kinds + view_ref target), never props.
    let outline = registry
        .dispatch("read_ui", json!({ "id": ui_id, "outline": true }), &ctx)
        .await
        .expect("outline read");
    assert_eq!(outline["views"].as_array().map(Vec::len), Some(2));
    let main_root = &outline["views"][0]["root"];
    assert_eq!(main_root["id"].as_str(), Some("root"));
    assert_eq!(main_root["kind"].as_str(), Some("stack"));
    assert!(main_root["props"].is_null(), "outline carries no props");
    let embed = &main_root["children"][1];
    assert_eq!(embed["kind"].as_str(), Some("view_ref"));
    assert_eq!(embed["view"].as_str(), Some("frag"));

    // One view ("file") — just its subtree plus the navigation meta.
    let frag = registry
        .dispatch("read_ui", json!({ "id": ui_id, "view": "frag" }), &ctx)
        .await
        .expect("view read");
    assert_eq!(frag["view"]["id"].as_str(), Some("frag"));
    assert_eq!(
        frag["view"]["root"]["children"][0]["id"].as_str(),
        Some("frag_txt")
    );
    assert!(
        frag.get("definition").is_none(),
        "scoped read omits the full body"
    );
    assert_eq!(
        frag["views"].as_array().map(Vec::len),
        Some(2),
        "meta still lists every view"
    );

    // One node subtree, found across views.
    let node = registry
        .dispatch(
            "read_ui",
            json!({ "id": ui_id, "node_id": "frag_txt" }),
            &ctx,
        )
        .await
        .expect("node read");
    assert_eq!(node["node"]["props"]["text"].as_str(), Some("old"));

    // Unknown scope targets fail with a pointer at outline mode.
    assert!(registry
        .dispatch("read_ui", json!({ "id": ui_id, "view": "ghost" }), &ctx)
        .await
        .is_err());
    assert!(registry
        .dispatch("read_ui", json!({ "id": ui_id, "node_id": "ghost" }), &ctx)
        .await
        .is_err());

    // replace_node rewrites one fragment in place (position kept, subtree swapped).
    registry
        .dispatch(
            "edit_ui",
            json!({
                "id": ui_id,
                "patch": [ { "op": "replace_node", "node_id": "frag_txt",
                             "node": { "id": "frag_txt", "kind": "text",
                                       "props": { "text": "new" } } } ]
            }),
            &ctx,
        )
        .await
        .expect("replace_node edit");
    let after = registry
        .dispatch(
            "read_ui",
            json!({ "id": ui_id, "node_id": "frag_txt" }),
            &ctx,
        )
        .await
        .expect("node read after");
    assert_eq!(after["node"]["props"]["text"].as_str(), Some("new"));
    assert_eq!(after["version"].as_i64(), Some(2));
}

#[tokio::test]
async fn read_object_returns_extracted_text_and_flags_absence() {
    let Some(url) = db_url() else {
        eprintln!("skipping read_object test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    use catalerum_core::model::{ConnectionKind, Role};
    use catalerum_store::UpsertObject;

    let store = Store::connect(&url).await.expect("store");
    let ws = store
        .workspaces()
        .create("readobj", &format!("readobj-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let conn = store
        .connections()
        .ensure(ws.id, ConnectionKind::Storage, "storage", None, None)
        .await
        .expect("conn");
    let bucket = store
        .buckets()
        .ensure(ws.id, conn.id, "files", None)
        .await
        .expect("bucket");
    let mk = |key: &'static str, ct: &'static str| UpsertObject {
        workspace_id: ws.id,
        bucket_id: bucket.id,
        key,
        size: 22,
        content_type: Some(ct),
        etag: None,
        last_modified: chrono::Utc::now(),
        sha256: None,
    };
    let with_text = store
        .objects()
        .upsert(&mk("docs/contract.txt", "text/plain"))
        .await
        .expect("object");
    store
        .documents()
        .upsert_by_source(
            ws.id,
            &SourceRef::Object { id: with_text.id },
            "the full contract text",
            None,
        )
        .await
        .expect("doc");
    let no_text = store
        .objects()
        .upsert(&mk("docs/logo.png", "image/png"))
        .await
        .expect("object2");

    let registry = build_registry(
        &store,
        None,
        NoteIngest::new(store.clone(), false, false),
        None,
        None,
        None,
        Vec::new(),
        None,
        None,
    );
    // A Viewer holds storage:read → may read an object's text.
    let ctx = ToolContext {
        workspace_id: Some(ws.id),
        capabilities: Some(catalerum_iam::base_capabilities(Role::Viewer)),
        ..Default::default()
    };

    // The catalogued document's full text comes back with its key.
    let out = registry
        .dispatch("read_object", json!({ "id": with_text.id }), &ctx)
        .await
        .expect("read_object");
    assert_eq!(out["text"], json!("the full contract text"));
    assert_eq!(out["has_text"], json!(true));
    assert_eq!(out["truncated"], json!(false), "short text isn't truncated");
    assert_eq!(out["key"], json!("docs/contract.txt"));

    // An object with no extracted text → has_text false, empty text (not error).
    let out2 = registry
        .dispatch("read_object", json!({ "id": no_text.id }), &ctx)
        .await
        .expect("read_object no text");
    assert_eq!(out2["has_text"], json!(false));
    assert_eq!(out2["text"], json!(""));

    // A nonexistent id errors (NotFound — never leaks another tenant's blob).
    assert!(
        registry
            .dispatch(
                "read_object",
                json!({ "id": uuid::Uuid::new_v4().to_string() }),
                &ctx,
            )
            .await
            .is_err(),
        "an unknown object id errors"
    );
}

#[tokio::test]
async fn search_files_finds_objects_by_extracted_text() {
    let Some(url) = db_url() else {
        eprintln!("skipping search_files test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    use catalerum_core::model::{ConnectionKind, Role};
    use catalerum_store::UpsertObject;

    let store = Store::connect(&url).await.expect("store");
    let ws = store
        .workspaces()
        .create(
            "searchfiles",
            &format!("searchfiles-{}", uuid::Uuid::new_v4()),
        )
        .await
        .expect("ws");
    let conn = store
        .connections()
        .ensure(ws.id, ConnectionKind::Storage, "storage", None, None)
        .await
        .expect("conn");
    let bucket = store
        .buckets()
        .ensure(ws.id, conn.id, "files", None)
        .await
        .expect("bucket");
    let mk = |key: &'static str| UpsertObject {
        workspace_id: ws.id,
        bucket_id: bucket.id,
        key,
        size: 0,
        content_type: Some("text/plain"),
        etag: None,
        last_modified: chrono::Utc::now(),
        sha256: None,
    };
    // An ingested object whose text contains the term: catalogue → store text →
    // link the FK (the join `search_text_in_workspace` filters on).
    let hit = store
        .objects()
        .upsert(&mk("docs/invoice.txt"))
        .await
        .expect("o1");
    let doc = store
        .documents()
        .upsert_by_source(
            ws.id,
            &SourceRef::Object { id: hit.id },
            "Invoice for order PO-12345, net 30.",
            None,
        )
        .await
        .expect("doc");
    store
        .objects()
        .set_extracted_text(ws.id, hit.id, Some(doc.id))
        .await
        .expect("link");
    // Another ingested object without the term — must not match.
    let other = store
        .objects()
        .upsert(&mk("docs/notes.txt"))
        .await
        .expect("o2");
    let other_doc = store
        .documents()
        .upsert_by_source(
            ws.id,
            &SourceRef::Object { id: other.id },
            "unrelated text",
            None,
        )
        .await
        .expect("doc2");
    store
        .objects()
        .set_extracted_text(ws.id, other.id, Some(other_doc.id))
        .await
        .expect("link2");

    let registry = build_registry(
        &store,
        None,
        NoteIngest::new(store.clone(), false, false),
        None,
        None,
        None,
        Vec::new(),
        None,
        None,
    );
    // A Viewer holds storage:read → may search file contents.
    let ctx = ToolContext {
        workspace_id: Some(ws.id),
        capabilities: Some(catalerum_iam::base_capabilities(Role::Viewer)),
        ..Default::default()
    };

    // A literal substring (case-insensitive) finds the one matching file, with the
    // object id (for read_object), key, and a match-windowed excerpt.
    let out = registry
        .dispatch("search_files", json!({ "query": "po-12345" }), &ctx)
        .await
        .expect("search_files");
    let results = out["results"].as_array().expect("results array");
    assert_eq!(
        results.len(),
        1,
        "only the file containing the term matches"
    );
    assert_eq!(results[0]["id"], json!(hit.id));
    assert_eq!(results[0]["key"], json!("docs/invoice.txt"));
    assert!(
        results[0]["excerpt"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("po-12345"),
        "excerpt is windowed on the match: {:?}",
        results[0]["excerpt"]
    );

    // A term in no file → an empty result set (not an error).
    let none = registry
        .dispatch("search_files", json!({ "query": "zzzznotpresent" }), &ctx)
        .await
        .expect("search_files empty");
    assert!(none["results"].as_array().unwrap().is_empty());

    // A blank query is rejected (required_str), never "match everything".
    assert!(
        registry
            .dispatch("search_files", json!({ "query": "   " }), &ctx)
            .await
            .is_err(),
        "a blank query is rejected"
    );
}

#[tokio::test]
async fn search_messages_finds_chat_by_content() {
    let Some(url) = db_url() else {
        eprintln!("skipping search_messages test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    use catalerum_core::model::{MessageRole, Origin, Role};
    use catalerum_store::NewMessage;

    let store = Store::connect(&url).await.expect("store");
    let ws = store
        .workspaces()
        .create("searchmsg", &format!("searchmsg-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let conv = store
        .conversations()
        .create(ws.id, Some("Migration planning"), Origin::Web)
        .await
        .expect("conv");
    for (role, content) in [
        (
            MessageRole::User,
            "Should we run the MIGRATION this weekend?",
        ),
        (
            MessageRole::Assistant,
            "Yes — let's schedule it for Saturday.",
        ),
        (MessageRole::User, "What about the backups?"),
    ] {
        store
            .messages()
            .insert(&NewMessage::text(conv.id, role, content))
            .await
            .expect("insert");
    }

    let registry = build_registry(
        &store,
        None,
        NoteIngest::new(store.clone(), false, false),
        None,
        None,
        None,
        Vec::new(),
        None,
        None,
    );
    // A Viewer holds conversation:read → may search chat history.
    let ctx = ToolContext {
        workspace_id: Some(ws.id),
        capabilities: Some(catalerum_iam::base_capabilities(Role::Viewer)),
        ..Default::default()
    };

    // A literal (case-insensitive) term finds the one matching message, carrying
    // the conversation title, role, and a match-centred snippet.
    let out = registry
        .dispatch("search_messages", json!({ "query": "migration" }), &ctx)
        .await
        .expect("search_messages");
    let results = out["results"].as_array().expect("results array");
    assert_eq!(
        results.len(),
        1,
        "only the message containing the term matches"
    );
    assert_eq!(
        results[0]["conversation_title"],
        json!("Migration planning")
    );
    assert_eq!(results[0]["role"], json!("user"));
    assert_eq!(results[0]["conversation_id"], json!(conv.id));
    assert!(
        results[0]["snippet"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("migration"),
        "snippet carries the match: {:?}",
        results[0]["snippet"]
    );

    // A term in no message → empty results (not an error).
    let none = registry
        .dispatch("search_messages", json!({ "query": "zzznotsaid" }), &ctx)
        .await
        .expect("search_messages empty");
    assert!(none["results"].as_array().unwrap().is_empty());

    // A blank query is rejected (required_str), never "match everything".
    assert!(
        registry
            .dispatch("search_messages", json!({ "query": "  " }), &ctx)
            .await
            .is_err(),
        "a blank query is rejected"
    );
}

#[tokio::test]
async fn read_conversation_returns_the_thread_oldest_first_and_is_tenant_scoped() {
    let Some(url) = db_url() else {
        eprintln!(
            "skipping read_conversation test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
        );
        return;
    };
    use catalerum_core::model::{MessageRole, Origin, Role};
    use catalerum_store::NewMessage;

    let store = Store::connect(&url).await.expect("store");
    let ws = store
        .workspaces()
        .create("readconv", &format!("readconv-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let conv = store
        .conversations()
        .create(ws.id, Some("Migration planning"), Origin::Web)
        .await
        .expect("conv");
    for (role, content) in [
        (MessageRole::User, "first: should we migrate?"),
        (MessageRole::Assistant, "second: yes, on Saturday"),
        (MessageRole::User, "third: and the backups?"),
    ] {
        store
            .messages()
            .insert(&NewMessage::text(conv.id, role, content))
            .await
            .expect("insert");
    }
    // Another workspace's conversation — reading it through `ws` must NOT leak.
    let other = store
        .workspaces()
        .create(
            "readconv-b",
            &format!("readconv-b-{}", uuid::Uuid::new_v4()),
        )
        .await
        .expect("other ws");
    let other_conv = store
        .conversations()
        .create(other.id, Some("Secret"), Origin::Web)
        .await
        .expect("other conv");

    let registry = build_registry(
        &store,
        None,
        NoteIngest::new(store.clone(), false, false),
        None,
        None,
        None,
        Vec::new(),
        None,
        None,
    );
    let ctx = ToolContext {
        workspace_id: Some(ws.id),
        capabilities: Some(catalerum_iam::base_capabilities(Role::Viewer)),
        ..Default::default()
    };

    // The thread comes back with its title + messages oldest-first.
    let out = registry
        .dispatch("read_conversation", json!({ "id": conv.id }), &ctx)
        .await
        .expect("read_conversation");
    assert_eq!(out["title"], json!("Migration planning"));
    let msgs = out["messages"].as_array().expect("messages array");
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[0]["role"], json!("user"));
    assert_eq!(msgs[0]["content"], json!("first: should we migrate?"));
    assert_eq!(msgs[1]["role"], json!("assistant"));
    assert_eq!(msgs[2]["content"], json!("third: and the backups?"));
    assert_eq!(msgs[0]["truncated"], json!(false));

    // A `limit` returns only the most recent N, still oldest-first.
    let out2 = registry
        .dispatch(
            "read_conversation",
            json!({ "id": conv.id, "limit": 2 }),
            &ctx,
        )
        .await
        .expect("read_conversation limited");
    let msgs2 = out2["messages"].as_array().unwrap();
    assert_eq!(msgs2.len(), 2, "only the 2 most recent");
    assert_eq!(msgs2[0]["content"], json!("second: yes, on Saturday"));
    assert_eq!(msgs2[1]["content"], json!("third: and the backups?"));

    // Another tenant's conversation id → NotFound (never leaks its messages).
    assert!(
        registry
            .dispatch("read_conversation", json!({ "id": other_conv.id }), &ctx)
            .await
            .is_err(),
        "a cross-workspace conversation id is refused"
    );
    // A nonexistent id → error.
    assert!(
        registry
            .dispatch(
                "read_conversation",
                json!({ "id": uuid::Uuid::new_v4().to_string() }),
                &ctx,
            )
            .await
            .is_err(),
        "an unknown conversation id errors"
    );
}

#[tokio::test]
async fn board_rename_keeps_columns_and_delete_cascades_tasks() {
    let Some(url) = db_url() else {
        eprintln!("skipping board rename/delete test: set CATALERUM_TEST_DATABASE_URL");
        return;
    };
    let store = Store::connect(&url).await.expect("store");
    let ws = store
        .workspaces()
        .create("brd", &format!("brd-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let board = store.boards().create(ws.id, "Sprint", &[]).await.unwrap();
    let col = board.columns[0].id;
    let task = store
        .tasks()
        .create(ws.id, board.id, col, "card", "", None)
        .await
        .unwrap();

    // Rename keeps the id + columns, just changes the name.
    let renamed = store
        .boards()
        .rename(ws.id, board.id, "Renamed")
        .await
        .unwrap();
    assert_eq!(renamed.id, board.id);
    assert_eq!(renamed.name, "Renamed");
    assert_eq!(renamed.columns.len(), board.columns.len());

    // Delete cascades the board's columns + tasks (the `0008` FKs).
    store.boards().delete(ws.id, board.id).await.unwrap();
    assert!(
        store.boards().get(ws.id, board.id).await.is_err(),
        "board gone after delete"
    );
    assert!(
        store.tasks().get(ws.id, task.id).await.is_err(),
        "the board's task cascaded"
    );
    // A second delete (now missing) errors.
    assert!(store.boards().delete(ws.id, board.id).await.is_err());
}

#[tokio::test]
async fn task_tools_drive_the_kanban_flow_and_are_capability_gated() {
    let Some(db) = db_url() else {
        eprintln!("skipping task-tools test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    use catalerum_core::model::Role;
    use catalerum_core::UserId;

    let store = Store::connect(&db).await.expect("store");
    let ws = store
        .workspaces()
        .create("tasktools", &format!("tasktools-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let board = store
        .boards()
        .create(ws.id, "Sprint", &[])
        .await
        .expect("board");
    let backlog = board.columns[0].id;
    let doing = board.columns[2].id;
    let registry = build_registry(
        &store,
        None,
        NoteIngest::new(store.clone(), false, false),
        None,
        None,
        None,
        Vec::new(),
        None,
        None,
    );
    let ctx = |role: Role| ToolContext {
        workspace_id: Some(ws.id),
        user_id: Some(UserId::new()),
        capabilities: Some(catalerum_iam::base_capabilities(role)),
        ..Default::default()
    };

    // A Viewer may read the next task but not create one (deny-by-default §19).
    assert!(matches!(
        registry
            .dispatch(
                "kanban_create_task",
                json!({ "board_id": board.id, "title": "x" }),
                &ctx(Role::Viewer)
            )
            .await,
        Err(Error::Unauthorized(_))
    ));

    // Member creates a task (defaults to the first column).
    let created = registry
        .dispatch(
            "kanban_create_task",
            json!({ "board_id": board.id, "title": "deploy" }),
            &ctx(Role::Member),
        )
        .await
        .expect("kanban_create_task");
    let task_id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["column_id"], json!(backlog.to_string()));
    assert_eq!(created["status"], json!("open"));

    // next_task (a read) returns it — allowed for a Viewer too.
    let next = registry
        .dispatch(
            "kanban_next_task",
            json!({ "column_id": backlog }),
            &ctx(Role::Viewer),
        )
        .await
        .expect("kanban_next_task");
    assert_eq!(next["id"], json!(task_id));

    // Move it to Doing, then complete it.
    let moved = registry
        .dispatch(
            "kanban_move_task",
            json!({ "task_id": task_id, "column_id": doing }),
            &ctx(Role::Member),
        )
        .await
        .expect("kanban_move_task");
    assert_eq!(moved["column_id"], json!(doing.to_string()));

    // set_task_status reaches the lifecycle states complete_task can't (status
    // is independent of the column move above). A Viewer is write-gated; a
    // Member sets in_progress then blocked; an unknown status is rejected.
    assert!(matches!(
        registry
            .dispatch(
                "kanban_set_task_status",
                json!({ "task_id": task_id, "status": "in_progress" }),
                &ctx(Role::Viewer)
            )
            .await,
        Err(Error::Unauthorized(_))
    ));
    let started = registry
        .dispatch(
            "kanban_set_task_status",
            json!({ "task_id": task_id, "status": "in_progress" }),
            &ctx(Role::Member),
        )
        .await
        .expect("set in_progress");
    assert_eq!(started["status"], json!("in_progress"));
    let blocked = registry
        .dispatch(
            "kanban_set_task_status",
            json!({ "task_id": task_id, "status": "blocked" }),
            &ctx(Role::Member),
        )
        .await
        .expect("set blocked");
    assert_eq!(blocked["status"], json!("blocked"));
    assert!(
        registry
            .dispatch(
                "kanban_set_task_status",
                json!({ "task_id": task_id, "status": "frozen" }),
                &ctx(Role::Member),
            )
            .await
            .is_err(),
        "an unknown status is rejected"
    );

    let done = registry
        .dispatch(
            "kanban_complete_task",
            json!({ "task_id": task_id }),
            &ctx(Role::Member),
        )
        .await
        .expect("kanban_complete_task");
    assert_eq!(done["status"], json!("done"));

    // The column is now empty of workable tasks → next_task is null.
    let empty = registry
        .dispatch(
            "kanban_next_task",
            json!({ "column_id": doing }),
            &ctx(Role::Member),
        )
        .await
        .expect("next_task empty");
    assert!(empty.is_null());

    // edit_task changes title/body in place; status + column are untouched. A
    // Viewer is write-gated; a Member edits; a blank title is rejected.
    assert!(matches!(
        registry
            .dispatch(
                "kanban_edit_task",
                json!({ "task_id": task_id, "title": "renamed" }),
                &ctx(Role::Viewer)
            )
            .await,
        Err(Error::Unauthorized(_))
    ));
    let edited = registry
        .dispatch(
            "kanban_edit_task",
            json!({ "task_id": task_id, "title": "ship it v2", "body": "more detail" }),
            &ctx(Role::Member),
        )
        .await
        .expect("kanban_edit_task");
    assert_eq!(edited["title"], json!("ship it v2"));
    assert_eq!(edited["status"], json!("done"), "edit preserves status");
    assert!(
        registry
            .dispatch(
                "kanban_edit_task",
                json!({ "task_id": task_id, "title": "   " }),
                &ctx(Role::Member),
            )
            .await
            .is_err(),
        "a blank title is rejected"
    );

    // delete_task removes the card entirely. A Viewer is write-gated; a Member
    // deletes; a second delete of the same id 404s (gone, not silently ok).
    assert!(matches!(
        registry
            .dispatch(
                "kanban_delete_task",
                json!({ "task_id": task_id }),
                &ctx(Role::Viewer)
            )
            .await,
        Err(Error::Unauthorized(_))
    ));
    let deleted = registry
        .dispatch(
            "kanban_delete_task",
            json!({ "task_id": task_id }),
            &ctx(Role::Member),
        )
        .await
        .expect("kanban_delete_task");
    assert_eq!(deleted["deleted"], json!(task_id));
    assert!(
        registry
            .dispatch(
                "kanban_delete_task",
                json!({ "task_id": task_id }),
                &ctx(Role::Member),
            )
            .await
            .is_err(),
        "deleting an already-deleted task errors"
    );

    // --- Name-based addressing: the whole flow without a single id lookup. ---
    // A Viewer can't create a board; a Member creates one with custom columns.
    assert!(matches!(
        registry
            .dispatch(
                "kanban_create_board",
                json!({ "name": "Tooling" }),
                &ctx(Role::Viewer)
            )
            .await,
        Err(Error::Unauthorized(_))
    ));
    let created_board = registry
        .dispatch(
            "kanban_create_board",
            json!({ "name": "Tooling", "columns": ["Now", "Later"] }),
            &ctx(Role::Member),
        )
        .await
        .expect("kanban_create_board");
    assert_eq!(created_board["columns"][0]["name"], json!("Now"));

    // Create by board name (case-insensitive) → lands in the first column.
    let first = registry
        .dispatch(
            "kanban_create_task",
            json!({ "board": "tooling", "title": "first" }),
            &ctx(Role::Member),
        )
        .await
        .expect("create by board name");
    assert_eq!(
        first["column_id"], created_board["columns"][0]["id"],
        "defaults to the board's first column"
    );
    let second = registry
        .dispatch(
            "kanban_create_task",
            json!({ "board": "Tooling", "column": "now", "title": "second" }),
            &ctx(Role::Member),
        )
        .await
        .expect("create by board+column names");
    let second_id = second["id"].as_str().unwrap().to_string();

    // A positioned same-column move reorders: "second" jumps to the top.
    let moved = registry
        .dispatch(
            "kanban_move_task",
            json!({ "task_id": second_id, "column": "Now", "position": 0 }),
            &ctx(Role::Member),
        )
        .await
        .expect("positioned move by column name");
    assert_eq!(moved["order"], json!(0));

    // next_task resolves board+column names too.
    let next_by_name = registry
        .dispatch(
            "kanban_next_task",
            json!({ "board": "Tooling", "column": "Now" }),
            &ctx(Role::Viewer),
        )
        .await
        .expect("next by names");
    assert_eq!(next_by_name["id"], json!(second_id));

    // Unknown names fail with a self-correction hint (the known names).
    let unknown_board = registry
        .dispatch(
            "kanban_create_task",
            json!({ "board": "nope", "title": "x" }),
            &ctx(Role::Member),
        )
        .await;
    assert!(
        unknown_board
            .as_ref()
            .err()
            .is_some_and(|e| e.to_string().contains("Tooling")),
        "an unknown board name lists the boards that exist: {unknown_board:?}"
    );
    let unknown_column = registry
        .dispatch(
            "kanban_move_task",
            json!({ "task_id": second_id, "column": "Someday" }),
            &ctx(Role::Member),
        )
        .await;
    assert!(
        unknown_column
            .as_ref()
            .err()
            .is_some_and(|e| e.to_string().contains("Now")),
        "an unknown column name lists the board's columns: {unknown_column:?}"
    );
    // Naming neither the board nor its id is rejected up front.
    assert!(registry
        .dispatch(
            "kanban_create_task",
            json!({ "title": "x" }),
            &ctx(Role::Member)
        )
        .await
        .is_err());

    // Partial edits: body-only keeps the title, title-only keeps the body,
    // and an edit with nothing to change is rejected.
    let body_only = registry
        .dispatch(
            "kanban_edit_task",
            json!({ "task_id": second_id, "body": "detail" }),
            &ctx(Role::Member),
        )
        .await
        .expect("body-only edit");
    assert_eq!(body_only["title"], json!("second"));
    assert_eq!(body_only["body_md"], json!("detail"));
    let title_only = registry
        .dispatch(
            "kanban_edit_task",
            json!({ "task_id": second_id, "title": "second v2" }),
            &ctx(Role::Member),
        )
        .await
        .expect("title-only edit");
    assert_eq!(title_only["body_md"], json!("detail"));
    assert!(registry
        .dispatch(
            "kanban_edit_task",
            json!({ "task_id": second_id }),
            &ctx(Role::Member)
        )
        .await
        .is_err());
}

/// A fake [`Executor`] that echoes the argv — no real process.
struct FakeExecutor;

#[async_trait]
impl Executor for FakeExecutor {
    async fn run(&self, cmd: CommandSpec) -> Result<catalerum_core::provider::CommandResult> {
        Ok(catalerum_core::provider::CommandResult {
            exit_code: 0,
            stdout: cmd.argv.join(" "),
            stderr: String::new(),
            timed_out: false,
        })
    }
    async fn open_session(
        &self,
        _spec: catalerum_core::provider::SessionSpec,
    ) -> Result<catalerum_core::provider::Session> {
        Err(Error::Unsupported("no sessions".into()))
    }
}

#[tokio::test]
async fn run_command_is_protected_and_requires_exec_run() {
    let Some(db) = db_url() else {
        eprintln!("skipping run_command test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    use catalerum_core::model::Role;

    let store = Store::connect(&db).await.expect("store");
    let registry = build_registry(
        &store,
        None,
        NoteIngest::new(store.clone(), false, false),
        None,
        None,
        Some(Arc::new(FakeExecutor)),
        Vec::new(),
        None,
        None,
    );
    let ws = WorkspaceId::new();
    let base = |caps: Option<Vec<Capability>>| ToolContext {
        workspace_id: Some(ws),
        capabilities: caps,
        ..Default::default()
    };
    let cmd = json!({ "command": ["echo", "hi"] });

    // No base role grants exec → denied for Member and Viewer (deny-by-default).
    for role in [Role::Member, Role::Viewer] {
        let denied = registry
            .dispatch(
                "run_command",
                cmd.clone(),
                &base(Some(catalerum_iam::base_capabilities(role))),
            )
            .await;
        assert!(
            matches!(denied, Err(Error::Unauthorized(_))),
            "{role:?} must be denied run_command"
        );
    }

    // An explicit exec:run capability (a deliberately-handed grant) → runs.
    let exec_cap = vec![Capability::new(Action::Run, Resource::domain("exec"))];
    let out = registry
        .dispatch("run_command", cmd.clone(), &base(Some(exec_cap.clone())))
        .await
        .expect("run_command with exec:run");
    assert_eq!(out["stdout"], json!("echo hi"));
    assert_eq!(out["exit_code"], json!(0));

    // Empty command is rejected (after the cap check passes).
    assert!(registry
        .dispatch(
            "run_command",
            json!({ "command": [] }),
            &base(Some(exec_cap))
        )
        .await
        .is_err());

    // Without an executor configured, the tool isn't even registered.
    let no_exec = build_registry(
        &store,
        None,
        NoteIngest::new(store.clone(), false, false),
        None,
        None,
        None,
        Vec::new(),
        None,
        None,
    );
    assert!(!no_exec.contains("run_command"));
}

#[tokio::test]
async fn skill_tools_enforce_per_skill_capability_selectors() {
    let Some(db) = db_url() else {
        eprintln!("skipping skill-tools test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    use catalerum_core::model::Role;
    use catalerum_core::UserId;

    let store = Store::connect(&db).await.expect("store");
    let ws = store
        .workspaces()
        .create(
            "skilltools",
            &format!("skilltools-{}", uuid::Uuid::new_v4()),
        )
        .await
        .expect("ws");
    // Two skills so a narrow `skill:use@summarize` grant can be shown to admit
    // one and deny the other.
    let mk = |name: &str| catalerum_store::NewSkill {
        name: name.to_string(),
        description: format!("{name} skill"),
        instructions_md: "# runbook".into(),
        tools: vec!["read_note".into()],
        code: None,
        advertised: true,
    };
    store
        .skills()
        .create(ws.id, &mk("summarize"))
        .await
        .expect("summarize");
    store
        .skills()
        .create(ws.id, &mk("triage-inbox"))
        .await
        .expect("triage-inbox");

    let registry = build_registry(
        &store,
        None,
        NoteIngest::new(store.clone(), false, false),
        None,
        None,
        None,
        Vec::new(),
        None,
        None,
    );
    let ctx = |caps: Option<Vec<Capability>>| ToolContext {
        workspace_id: Some(ws.id),
        user_id: Some(UserId::new()),
        capabilities: caps,
        ..Default::default()
    };
    let member = || ctx(Some(catalerum_iam::base_capabilities(Role::Member)));
    let viewer = || ctx(Some(catalerum_iam::base_capabilities(Role::Viewer)));
    let use_skill = |name: &str| json!({ "name": name });

    // A Member holds whole-domain `skill:use`, so use_skill works for any skill
    // and list_skills (Read@skill) returns both.
    let listed = registry
        .dispatch("list_skills", json!({}), &member())
        .await
        .expect("list");
    assert_eq!(listed["skills"].as_array().unwrap().len(), 2);
    let used = registry
        .dispatch("use_skill", use_skill("summarize"), &member())
        .await
        .expect("member uses summarize");
    assert_eq!(used["name"], json!("summarize"));
    assert_eq!(used["instructions"], json!("# runbook"));
    assert_eq!(used["tools"], json!(["read_note"]));
    assert!(registry
        .dispatch("use_skill", use_skill("triage-inbox"), &member())
        .await
        .is_ok());

    // A Viewer may discover skills (Read@skill) but holds no `skill:use`, so the
    // per-skill check inside invoke denies invocation (deny-by-default §19).
    assert!(registry
        .dispatch("list_skills", json!({}), &viewer())
        .await
        .is_ok());
    assert!(matches!(
        registry
            .dispatch("use_skill", use_skill("summarize"), &viewer())
            .await,
        Err(Error::Unauthorized(_))
    ));

    // A narrow `skill:use@summarize` grant (a per-resource selector, §19) admits
    // exactly that skill and denies any other — the headline of this vertical.
    let narrow = || {
        ctx(Some(vec![Capability::new(
            Action::Use,
            Resource::new("skill", "summarize"),
        )]))
    };
    assert!(registry
        .dispatch("use_skill", use_skill("summarize"), &narrow())
        .await
        .is_ok());
    assert!(matches!(
        registry
            .dispatch("use_skill", use_skill("triage-inbox"), &narrow())
            .await,
        Err(Error::Unauthorized(_))
    ));
    // That grant carries no Read@skill, so the dispatch gate denies list_skills.
    assert!(matches!(
        registry.dispatch("list_skills", json!({}), &narrow()).await,
        Err(Error::Unauthorized(_))
    ));

    // An unknown skill is a bad request, not an authz failure (the lookup fails
    // before the capability check) — even for a fully-authorized Member.
    assert!(matches!(
        registry
            .dispatch("use_skill", use_skill("nope"), &member())
            .await,
        Err(Error::Invalid(_))
    ));

    // No capabilities supplied → enforcement off (internal/legacy caller path).
    let no_caps = ctx(None);
    assert!(registry
        .dispatch("use_skill", use_skill("summarize"), &no_caps)
        .await
        .is_ok());
    assert!(registry
        .dispatch("list_skills", json!({}), &no_caps)
        .await
        .is_ok());

    // Missing workspace is rejected.
    assert!(registry
        .dispatch("use_skill", use_skill("summarize"), &ToolContext::default())
        .await
        .is_err());
}

#[tokio::test]
async fn create_and_edit_skill_tools_write_the_library() {
    let Some(db) = db_url() else {
        eprintln!(
            "skipping skill-write-tools test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
        );
        return;
    };
    use catalerum_core::model::Role;
    use catalerum_core::UserId;

    let store = Store::connect(&db).await.expect("store");
    let ws = store
        .workspaces()
        .create(
            "skillwrite",
            &format!("skillwrite-{}", uuid::Uuid::new_v4()),
        )
        .await
        .expect("ws");
    let registry = build_registry(
        &store,
        None,
        NoteIngest::new(store.clone(), false, false),
        None,
        None,
        None,
        Vec::new(),
        None,
        None,
    );
    let ctx = |caps: Option<Vec<Capability>>| ToolContext {
        workspace_id: Some(ws.id),
        user_id: Some(UserId::new()),
        capabilities: caps,
        ..Default::default()
    };
    let member = || ctx(Some(catalerum_iam::base_capabilities(Role::Member)));
    let viewer = || ctx(Some(catalerum_iam::base_capabilities(Role::Viewer)));

    // A Member (skill:write) creates a skill; name/description are trimmed and
    // the tool list normalized (trim / de-dup) like the REST route's.
    let created = registry
        .dispatch(
            "create_skill",
            json!({
                "name": "  summarize  ",
                "description": " Summarize a thread ",
                "instructions_md": "# runbook",
                "tools": [" read_note ", "read_note", ""],
            }),
            &member(),
        )
        .await
        .expect("create");
    assert_eq!(created["name"], json!("summarize"));
    assert_eq!(created["description"], json!("Summarize a thread"));
    assert_eq!(created["tools"], json!(["read_note"]));
    assert_eq!(created["has_code"], json!(false));

    // The name is the per-workspace unique key — a duplicate create conflicts
    // instead of silently replacing (that's edit_skill's job).
    assert!(matches!(
        registry
            .dispatch("create_skill", json!({ "name": "summarize" }), &member())
            .await,
        Err(Error::Conflict(_))
    ));

    // A Viewer holds no `skill:write` → the dispatch gate denies both writes
    // (deny-by-default §19).
    assert!(matches!(
        registry
            .dispatch("create_skill", json!({ "name": "nope" }), &viewer())
            .await,
        Err(Error::Unauthorized(_))
    ));
    assert!(matches!(
        registry
            .dispatch("edit_skill", json!({ "name": "summarize" }), &viewer())
            .await,
        Err(Error::Unauthorized(_))
    ));

    // edit_skill is PARTIAL: new description + attached code, while the omitted
    // runbook and tool set keep their stored values.
    let edited = registry
        .dispatch(
            "edit_skill",
            json!({
                "name": "summarize",
                "description": "v2",
                "code": { "language": "python", "source": "print(1)" },
            }),
            &member(),
        )
        .await
        .expect("edit");
    assert_eq!(edited["description"], json!("v2"));
    assert_eq!(edited["tools"], json!(["read_note"]));
    assert_eq!(edited["has_code"], json!(true));
    let stored = store
        .skills()
        .get_by_name(ws.id, "summarize")
        .await
        .expect("get")
        .expect("exists");
    assert_eq!(stored.instructions_md, "# runbook", "omitted field kept");

    // An explicit `"code": null` clears the attached code (omitting keeps it).
    let cleared = registry
        .dispatch(
            "edit_skill",
            json!({ "name": "summarize", "code": null }),
            &member(),
        )
        .await
        .expect("clear code");
    assert_eq!(cleared["has_code"], json!(false));

    // Editing an unknown skill is a bad request, not an implicit create.
    assert!(matches!(
        registry
            .dispatch("edit_skill", json!({ "name": "ghost" }), &member())
            .await,
        Err(Error::Invalid(_))
    ));
}

#[tokio::test]
async fn moving_a_task_fires_a_matching_taskmoved_automation_end_to_end() {
    let Some(url) = db_url() else {
        eprintln!(
            "skipping move_task automation test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
        );
        return;
    };
    use catalerum_automation::{Action, ActionOutcome, ActionRunner};
    use catalerum_core::model::RunStatus;
    use catalerum_ingest::{AutomationContext, SyncWorker};

    struct OkRunner;
    #[async_trait::async_trait]
    impl ActionRunner for OkRunner {
        async fn run(
            &self,
            _ws: WorkspaceId,
            _action: &Action,
            _trigger: Option<&serde_json::Value>,
            _grant: Option<&catalerum_core::model::Grant>,
        ) -> ActionOutcome {
            ActionOutcome::succeeded(None)
        }
    }

    // Isolated db (own `job_queue`) so the worker below can't claim another
    // parallel test's `run_automation` job (and vice versa).
    let store = crate::test_db::isolated_store(&url).await;
    let ws = store
        .workspaces()
        .create("taskfire", &format!("taskfire-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let board = store
        .boards()
        .create(ws.id, "Sprint", &[])
        .await
        .expect("board");
    let backlog = board.columns[0].id;
    let doing = board
        .columns
        .iter()
        .find(|c| c.name == "Doing")
        .expect("Doing column");
    let task = store
        .tasks()
        .create(ws.id, board.id, backlog, "ship it", "", None)
        .await
        .expect("task");

    // An automation that fires when a task moves into "Doing" on "Sprint".
    let spec = catalerum_store::NewAutomation {
        name: "on-doing".into(),
        enabled: true,
        triggers: vec![json!({ "kind": "task_moved", "board": "Sprint", "to_column": "Doing" })],
        condition: None,
        actions: vec![json!({ "kind": "summarize" })],
        spec: None,
        grant_id: None,
    };
    let automation = store
        .automations()
        .create(ws.id, &spec)
        .await
        .expect("automation");

    // Move the task into "Doing" via the tool → the TaskMoved dispatch enqueues
    // a durable run_automation job.
    let tool = MoveTaskTool {
        store: store.clone(),
    };
    let ctx = ToolContext {
        workspace_id: Some(ws.id),
        ..Default::default()
    };
    tool.invoke(json!({ "task_id": task.id, "column_id": doing.id }), &ctx)
        .await
        .expect("kanban_move_task");

    // A worker with a runner drains the job → a run is recorded for the automation.
    let runner: std::sync::Arc<dyn ActionRunner> = std::sync::Arc::new(OkRunner);
    let worker =
        SyncWorker::new(store.clone()).with_automation_context(AutomationContext::new(runner));
    let mut fired = false;
    for _ in 0..50 {
        if !store
            .automation_runs()
            .list_runs(ws.id, automation.id, 5)
            .await
            .unwrap()
            .is_empty()
        {
            fired = true;
            break;
        }
        if !worker.poll_once().await.unwrap() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
    assert!(
        fired,
        "moving the task into Doing fired the automation (a run was recorded)"
    );
    let runs = store
        .automation_runs()
        .list_runs(ws.id, automation.id, 5)
        .await
        .unwrap();
    assert_eq!(runs[0].status, RunStatus::Succeeded);
    assert_eq!(
        runs[0].trigger.as_ref().unwrap()["to_column"],
        json!("Doing")
    );

    // A same-column "move" is not a transition → it must not re-fire (§24).
    tool.invoke(json!({ "task_id": task.id, "column_id": doing.id }), &ctx)
        .await
        .expect("re-move into the same column");
    for _ in 0..10 {
        if !worker.poll_once().await.unwrap() {
            break;
        }
    }
    assert_eq!(
        store
            .automation_runs()
            .list_runs(ws.id, automation.id, 5)
            .await
            .unwrap()
            .len(),
        1,
        "a same-column re-move does not fire the automation again"
    );
}

#[tokio::test]
async fn get_emails_operations_are_workspace_scoped() {
    let Some(url) = db_url() else {
        eprintln!("skipping get_emails test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    use catalerum_core::model::{ConnectionKind, Email, EmailAddress};
    use catalerum_core::EmailId;

    let store = Store::connect(&url).await.expect("store");
    let ws = store
        .workspaces()
        .create("ge", &format!("ge-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let conn = store
        .connections()
        .create(ws.id, ConnectionKind::Email, "maildir", None, None)
        .await
        .unwrap();
    let mb = store
        .mailboxes()
        .upsert(ws.id, conn.id, "/inbox", "INBOX", true)
        .await
        .unwrap();

    let mk = |uid: &str, from: &str, subject: &str, flags: Vec<String>| Email {
        id: EmailId::new(),
        workspace_id: ws.id,
        mailbox_id: mb.id,
        uid: uid.to_string(),
        message_id: None,
        from: Some(EmailAddress::new(from)),
        to: vec![],
        cc: vec![],
        subject: subject.to_string(),
        received_at: Some(chrono::Utc::now()),
        body_text: Some("body".into()),
        body_html: None,
        has_attachments: false,
        flags,
        labels: vec![],
        raw_ref: None,
        attachments: Vec::new(),
        raw: None,
    };
    // Two read, one unread (no "seen" flag).
    let e1 = store
        .emails()
        .upsert_by_uid(&mk("u1", "ada@example.com", "Engine", vec!["seen".into()]))
        .await
        .unwrap();
    store
        .emails()
        .upsert_by_uid(&mk("u2", "bob@example.org", "Lunch", vec!["seen".into()]))
        .await
        .unwrap();
    store
        .emails()
        .upsert_by_uid(&mk("u3", "ada@example.com", "Follow-up", vec![]))
        .await
        .unwrap();

    let tool = GetEmailsTool {
        store: store.clone(),
    };
    let ctx = ToolContext {
        workspace_id: Some(ws.id),
        ..Default::default()
    };

    // recent_emails → all three, carrying the mailbox name + sender.
    let recent = tool
        .invoke(json!({ "operation": "recent_emails" }), &ctx)
        .await
        .unwrap();
    let rows = recent["results"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|r| r["mailbox"] == json!("INBOX")));

    // emails_by_sender("ada") → the two from ada (case-insensitive substring).
    let by_sender = tool
        .invoke(
            json!({ "operation": "emails_by_sender", "sender": "ADA" }),
            &ctx,
        )
        .await
        .unwrap();
    let s = by_sender["results"].as_array().unwrap();
    assert_eq!(s.len(), 2);
    assert!(s
        .iter()
        .all(|r| r["from"].as_str().unwrap().contains("ada@example.com")));

    // unread_emails → only the message without a "seen" flag.
    let unread = tool
        .invoke(json!({ "operation": "unread_emails" }), &ctx)
        .await
        .unwrap();
    let u = unread["results"].as_array().unwrap();
    assert_eq!(u.len(), 1);
    assert_eq!(u[0]["subject"], json!("Follow-up"));
    assert_eq!(u[0]["unread"], json!(true));

    // untagged_emails → only mail with no labels, and each summary carries
    // the (mailbox_id, uid) a LabelEmail automation targets by.
    store
        .emails()
        .set_labels(ws.id, e1.id, &["work".to_string()])
        .await
        .unwrap();
    let untagged = tool
        .invoke(json!({ "operation": "untagged_emails" }), &ctx)
        .await
        .unwrap();
    let t = untagged["results"].as_array().unwrap();
    assert_eq!(t.len(), 2, "the labelled email drops out of the sweep");
    assert!(t.iter().all(|r| r["labels"] == json!([])));
    assert!(t.iter().all(|r| r["mailbox_id"] == json!(mb.id)));
    assert!(t
        .iter()
        .any(|r| r["uid"] == json!("u2") && r["subject"] == json!("Lunch")));
    assert!(t.iter().all(|r| r["uid"] != json!("u1")));

    // §18: another workspace sees nothing.
    let other = ToolContext {
        workspace_id: Some(WorkspaceId::new()),
        ..Default::default()
    };
    let none = tool
        .invoke(json!({ "operation": "recent_emails" }), &other)
        .await
        .unwrap();
    assert!(none["results"].as_array().unwrap().is_empty());

    // Bad inputs: unknown operation, missing workspace, by_sender without sender.
    assert!(tool
        .invoke(json!({ "operation": "wat" }), &ctx)
        .await
        .is_err());
    assert!(tool
        .invoke(
            json!({ "operation": "recent_emails" }),
            &ToolContext::default()
        )
        .await
        .is_err());
    assert!(tool
        .invoke(json!({ "operation": "emails_by_sender" }), &ctx)
        .await
        .is_err());

    // read_email returns one message's full body + headers by id (the id comes
    // from the summaries above). Same email:read gate as the lookups.
    let reader = ReadEmailTool {
        store: store.clone(),
    };
    let full = reader
        .invoke(json!({ "id": u[0]["id"].clone() }), &ctx)
        .await
        .expect("read_email");
    assert_eq!(full["subject"], json!("Follow-up"));
    assert_eq!(full["body"], json!("body"));
    assert_eq!(full["truncated"], json!(false));
    assert_eq!(full["unread"], json!(true));
    assert_eq!(full["mailbox"], json!("INBOX"));
    assert!(full["from"].as_str().unwrap().contains("ada@example.com"));
    // A bad id errors; another workspace can't read this id (§18 — NotFound).
    assert!(reader.invoke(json!({ "id": "nope" }), &ctx).await.is_err());
    assert!(reader
        .invoke(json!({ "id": u[0]["id"].clone() }), &other)
        .await
        .is_err());
}

/// The source-connection tools (SOUL §8/§10/§28): a chat can register an
/// email/JMAP source + a calendar source, list them (with the dormant flag +
/// per-kind capability gate), and the ids round-trip into collect triggers.
#[tokio::test]
async fn connection_tools_create_list_and_gate_by_kind() {
    let Some(url) = db_url() else {
        eprintln!(
            "skipping connection tools test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
        );
        return;
    };
    let store = Store::connect(&url).await.expect("store");
    let ws = store
        .workspaces()
        .create("ct", &format!("ct-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let ctx = ToolContext {
        workspace_id: Some(ws.id),
        ..Default::default()
    };

    // Register a JMAP source (the Fastmail shape) through the tool.
    let create_email = CreateEmailConnectionTool {
        store: store.clone(),
    };
    let created = create_email
        .invoke(
            json!({
                "provider": "jmap",
                "name": "Fastmail",
                "session_url": "https://api.fastmail.com/jmap/session",
                "token": "secret",
            }),
            &ctx,
        )
        .await
        .expect("create jmap connection");
    assert_eq!(created["kind"], json!("email"));
    assert_eq!(created["provider"], json!("jmap"));
    assert_eq!(created["collecting"], json!(false));
    let email_id = created["id"].as_str().unwrap().to_string();
    // The stored config blob is the exact REST shape (provider + settings).
    let row = store
        .connections()
        .get_row(ws.id, email_id.parse().unwrap())
        .await
        .unwrap();
    assert_eq!(row.config()["provider"], json!("jmap"));
    assert_eq!(row.config()["token"], json!("secret"));

    // A provider missing its required fields is rejected with the field name.
    let err = create_email
        .invoke(json!({ "provider": "jmap", "name": "broken" }), &ctx)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("session_url"), "got: {err}");

    // Register a calendar source too.
    let create_cal = CreateCalendarConnectionTool {
        store: store.clone(),
    };
    let cal = create_cal
        .invoke(
            json!({ "kind": "local", "name": "Team", "config": { "dir": "/srv/cal" } }),
            &ctx,
        )
        .await
        .expect("create calendar connection");
    assert_eq!(cal["provider"], json!("local"));
    // The flat spelling (the advertised schema) lands the same config blob.
    let flat = create_cal
        .invoke(
            json!({ "kind": "webcal", "name": "Feiertage",
                    "base_url": "https://example.org/feiertage.ics" }),
            &ctx,
        )
        .await
        .expect("create webcal connection from flat args");
    let row = store
        .connections()
        .get_row(ws.id, flat["id"].as_str().unwrap().parse().unwrap())
        .await
        .unwrap();
    assert_eq!(row.config()["provider"], json!("webcal"));
    assert_eq!(
        row.config()["base_url"],
        json!("https://example.org/feiertage.ics")
    );
    assert!(create_cal
        .invoke(
            json!({ "kind": "caldav", "name": "nope", "config": {} }),
            &ctx
        )
        .await
        .is_err());

    // list_connections is kind-scoped and reports the dormant flag; the config
    // (with its credentials) never appears.
    let list = ListConnectionsTool {
        store: store.clone(),
    };
    let emails = list.invoke(json!({ "kind": "email" }), &ctx).await.unwrap();
    let rows = emails["connections"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], json!(email_id));
    assert_eq!(rows[0]["collecting"], json!(false));
    assert!(rows[0].get("config").is_none());
    assert!(!emails.to_string().contains("secret"), "no secret leaks");
    let cals = list
        .invoke(json!({ "kind": "calendar" }), &ctx)
        .await
        .unwrap();
    assert_eq!(cals["connections"].as_array().unwrap().len(), 2);
    assert!(list
        .invoke(json!({ "kind": "storage" }), &ctx)
        .await
        .is_err());

    // Per-kind gate (§19): a calendar:read-only grant can list calendars but
    // not email sources through the same tool.
    let scoped = ToolContext {
        workspace_id: Some(ws.id),
        capabilities: Some(vec![Capability::new(
            Action::Read,
            Resource::domain("calendar"),
        )]),
        ..Default::default()
    };
    assert!(list
        .invoke(json!({ "kind": "calendar" }), &scoped)
        .await
        .is_ok());
    assert!(list
        .invoke(json!({ "kind": "email" }), &scoped)
        .await
        .is_err());

    // Once an enabled collect automation heads at the source, it lists live.
    store
        .automations()
        .create(
            ws.id,
            &catalerum_store::NewAutomation {
                name: "ingest".into(),
                enabled: true,
                triggers: vec![json!({ "kind": "collect_email", "connection": email_id })],
                condition: None,
                actions: vec![json!({ "kind": "write_email" })],
                spec: None,
                grant_id: None,
            },
        )
        .await
        .unwrap();
    let emails = list.invoke(json!({ "kind": "email" }), &ctx).await.unwrap();
    assert_eq!(emails["connections"][0]["collecting"], json!(true));
}

/// Authoring an automation whose collect trigger names a placeholder (or a
/// wrong-kind / foreign) connection fails with an actionable error, and a real
/// connection id passes (SOUL §10/§28 — the `fastmail` placeholder trap).
#[tokio::test]
async fn automation_authoring_rejects_bad_collect_connections() {
    let Some(url) = db_url() else {
        eprintln!(
            "skipping collect-connection validation test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
        );
        return;
    };
    let store = Store::connect(&url).await.expect("store");
    let ws = store
        .workspaces()
        .create("cv", &format!("cv-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let ctx = ToolContext {
        workspace_id: Some(ws.id),
        ..Default::default()
    };
    let tool = CreateAutomationTool {
        store: store.clone(),
    };
    let body = |connection: &str| {
        json!({
            "name": format!("ingest-{}", uuid::Uuid::new_v4()),
            "triggers": [{ "kind": "collect_email", "connection": connection }],
            "actions": [{ "kind": "write_email" }],
        })
    };

    // A placeholder name is rejected at save time, pointing at the fix.
    let err = tool.invoke(body("fastmail"), &ctx).await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("list_connections"), "actionable error: {msg}");
    assert!(msg.contains("create_email_connection"), "got: {msg}");

    // A well-formed uuid that doesn't exist in the workspace is rejected too.
    let ghost = catalerum_core::ConnectionId::new().to_string();
    assert!(tool.invoke(body(&ghost), &ctx).await.is_err());

    // A connection of the WRONG kind is rejected (a calendar source can't head
    // a collect_email).
    let cal = store
        .connections()
        .create(ws.id, ConnectionKind::Calendar, "cal", None, None)
        .await
        .unwrap();
    let err = tool
        .invoke(body(&cal.id.to_string()), &ctx)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("collects email"), "got: {err}");

    // The real thing passes, and update_automation validates the same way.
    let conn = store
        .connections()
        .create(ws.id, ConnectionKind::Email, "src", None, None)
        .await
        .unwrap();
    let ok = tool
        .invoke(body(&conn.id.to_string()), &ctx)
        .await
        .expect("valid collect automation");
    let name = ok["name"].as_str().unwrap().to_string();
    let update = UpdateAutomationTool {
        store: store.clone(),
    };
    let mut replaced = body("fastmail");
    replaced["name"] = json!(name);
    assert!(update.invoke(replaced, &ctx).await.is_err());
}

/// `edit_automation` is a **partial** update: an omitted field keeps its stored
/// value (so editing only `enabled` never wipes a node graph — the trap of a
/// full-replacement `update_automation`), a present field replaces, an explicit
/// `null` clears the two nullable fields, and the §19 grant survives an edit.
#[tokio::test]
async fn edit_automation_is_partial_and_preserves_untouched_fields() {
    let Some(url) = db_url() else {
        eprintln!("skipping edit_automation test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    use catalerum_core::capability::Constraints;

    let store = Store::connect(&url).await.expect("store");
    let ws = store
        .workspaces()
        .create("ea", &format!("ea-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let ctx = ToolContext {
        workspace_id: Some(ws.id),
        ..Default::default()
    };
    let create = CreateAutomationTool {
        store: store.clone(),
    };
    let edit = EditAutomationTool {
        store: store.clone(),
    };

    // (1) A node-graph automation: a single schedule trigger under spec.graph.
    let graph = |cron: &str| {
        json!({ "graph": {
            "nodes": [{ "id": "t1", "kind": "trigger",
                        "trigger": { "kind": "schedule", "cron": cron } }],
            "edges": []
        } })
    };
    let gname = format!("graphed-{}", uuid::Uuid::new_v4());
    let made = create
        .invoke(
            json!({ "name": gname, "enabled": true, "spec": graph("0 9 * * *") }),
            &ctx,
        )
        .await
        .expect("create graph automation");
    assert_eq!(made["enabled"], json!(true));
    assert!(made["spec"]["graph"].is_object(), "graph persisted");
    assert_eq!(made["triggers"][0]["cron"], json!("0 9 * * *"));

    // Editing ONLY `enabled` keeps the graph (the headline: update_automation would
    // wipe spec here) and recompiles the dispatch trigger from spec.graph.
    let toggled = edit
        .invoke(json!({ "name": gname, "enabled": false }), &ctx)
        .await
        .expect("toggle enabled");
    assert_eq!(toggled["enabled"], json!(false));
    assert_eq!(
        toggled["spec"]["graph"]["nodes"][0]["trigger"]["cron"],
        json!("0 9 * * *"),
        "graph survives an enabled-only edit"
    );
    assert_eq!(toggled["triggers"][0]["cron"], json!("0 9 * * *"));

    // Editing ONLY `spec` swaps the graph; the earlier `enabled=false` is kept.
    let respec = edit
        .invoke(json!({ "name": gname, "spec": graph("30 6 * * 1") }), &ctx)
        .await
        .expect("replace spec");
    assert_eq!(respec["enabled"], json!(false), "enabled preserved");
    assert_eq!(respec["triggers"][0]["cron"], json!("30 6 * * 1"));

    // (2) A linear automation: editing only `actions` keeps the triggers, and an
    // explicit `condition: null` clears the stored condition.
    let lname = format!("linear-{}", uuid::Uuid::new_v4());
    create
        .invoke(
            json!({
                "name": lname,
                "triggers": [{ "kind": "schedule", "cron": "0 0 * * *" }],
                "condition": { "kind": "always" },
                "actions": [{ "kind": "create_note", "title": "a" }],
            }),
            &ctx,
        )
        .await
        .expect("create linear automation");
    let edited = edit
        .invoke(
            json!({ "name": lname, "actions": [{ "kind": "create_note", "title": "b" }] }),
            &ctx,
        )
        .await
        .expect("edit actions only");
    assert_eq!(
        edited["triggers"][0]["cron"],
        json!("0 0 * * *"),
        "triggers kept"
    );
    assert_eq!(
        edited["actions"][0]["title"],
        json!("b"),
        "actions replaced"
    );
    assert_eq!(
        edited["condition"],
        json!({ "kind": "always" }),
        "condition kept"
    );
    let cleared = edit
        .invoke(json!({ "name": lname, "condition": null }), &ctx)
        .await
        .expect("clear condition");
    assert!(
        cleared["condition"].is_null(),
        "condition cleared by explicit null"
    );
    assert_eq!(
        cleared["actions"][0]["title"],
        json!("b"),
        "actions still kept"
    );

    // (3) An unknown name is a clear error pointing at create_automation.
    let err = edit
        .invoke(json!({ "name": "ghost" }), &ctx)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("create_automation"), "got: {err}");

    // (4) The §19 grant an automation runs under survives a partial edit (unlike a
    // full-replacement update_automation, which nulls grant_id).
    let grant = store
        .grants()
        .upsert(
            ws.id,
            "reader",
            &[Capability::new(Action::Read, Resource::domain("note"))],
            &Constraints::default(),
        )
        .await
        .expect("grant");
    let granted = store
        .automations()
        .create(
            ws.id,
            &catalerum_store::NewAutomation {
                name: format!("granted-{}", uuid::Uuid::new_v4()),
                enabled: true,
                triggers: vec![json!({ "kind": "schedule", "cron": "0 12 * * *" })],
                condition: None,
                actions: vec![json!({ "kind": "create_note", "title": "x" })],
                spec: None,
                grant_id: Some(grant.id),
            },
        )
        .await
        .expect("granted automation");
    let after = edit
        .invoke(json!({ "name": granted.name, "enabled": false }), &ctx)
        .await
        .expect("edit granted automation");
    assert_eq!(
        after["grant_id"],
        json!(grant.id.to_string()),
        "grant preserved across a partial edit"
    );
}

#[test]
fn agent_profile_body_props_describes_the_full_body_minus_name() {
    let props = agent_profile_body_props();
    let obj = props.as_object().unwrap();
    for k in [
        "model",
        "system_prompt",
        "tools",
        "skills",
        "subagents",
        "channels",
        "grant_id",
    ] {
        assert!(obj.contains_key(k), "shared body is missing `{k}`");
    }
    // `name` is added (and required) by each tool, not part of the shared body.
    assert!(!obj.contains_key("name"));
}

#[test]
fn agent_profile_spec_trims_lists_and_blanks_to_none() {
    let args = json!({
        "model": "  ",                 // blank → absent
        "system_prompt": " be brief ",
        "tools": ["  get_emails ", "", "notify", "  "],
        "channels": ["telegram"],
    });
    let spec = agent_profile_spec(&args, "calbot".into(), None);
    assert_eq!(spec.name, "calbot");
    assert_eq!(spec.model, None);
    assert_eq!(spec.system_prompt.as_deref(), Some("be brief"));
    assert_eq!(
        spec.tools,
        vec!["get_emails".to_string(), "notify".to_string()]
    );
    assert!(spec.skills.is_empty());
    assert_eq!(spec.channels, vec!["telegram".to_string()]);
    assert!(spec.grant_id.is_none());
}

#[tokio::test]
async fn agent_profile_tools_crud_and_grant_attenuation() {
    let Some(url) = db_url() else {
        eprintln!(
            "skipping agent_profile tools test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
        );
        return;
    };
    use catalerum_core::capability::Constraints;

    let store = Store::connect(&url).await.expect("store");
    let ws = store
        .workspaces()
        .create("ap", &format!("ap-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");

    // Admin-equivalent authority: a single wildcard capability covers everything,
    // so any referenced grant attenuates (⊆) cleanly.
    let admin_ctx = ToolContext {
        workspace_id: Some(ws.id),
        capabilities: Some(vec![Capability::new(Action::Any, Resource::any())]),
        ..Default::default()
    };

    let create = CreateAgentProfileTool {
        store: store.clone(),
    };
    let get = GetAgentProfileTool {
        store: store.clone(),
    };
    let list = ListAgentProfilesTool {
        store: store.clone(),
    };
    let update = UpdateAgentProfileTool {
        store: store.clone(),
    };
    let delete = DeleteAgentProfileTool {
        store: store.clone(),
    };

    // Create a profile.
    let made = create
        .invoke(
            json!({
                "name": "calbot",
                "model": "claude-test",
                "tools": ["get_emails", "notify"],
                "channels": ["telegram"],
            }),
            &admin_ctx,
        )
        .await
        .unwrap();
    assert_eq!(made["name"], json!("calbot"));
    assert_eq!(made["model"], json!("claude-test"));

    // Get returns the full definition.
    let got = get
        .invoke(json!({ "name": "calbot" }), &admin_ctx)
        .await
        .unwrap();
    assert_eq!(got["channels"], json!(["telegram"]));
    assert_eq!(got["tools"].as_array().unwrap().len(), 2);

    // List summarises it (counts, no grant yet).
    let listed = list.invoke(json!({}), &admin_ctx).await.unwrap();
    let items = listed["agent_profiles"].as_array().unwrap();
    let row = items
        .iter()
        .find(|p| p["name"] == json!("calbot"))
        .expect("calbot in list");
    assert_eq!(row["tool_count"], json!(2));
    assert_eq!(row["has_grant"], json!(false));

    // Re-creating the same name conflicts (create is not create-or-replace).
    assert!(create
        .invoke(json!({ "name": "calbot" }), &admin_ctx)
        .await
        .is_err());

    // A grant ⊆ admin authority binds onto the profile via update (create-or-replace).
    let grant = store
        .grants()
        .upsert(
            ws.id,
            "cal-grant",
            &[Capability::new(Action::Read, Resource::domain("calendar"))],
            &Constraints::default(),
        )
        .await
        .unwrap();
    let gid = grant.id.into_uuid().to_string();
    let updated = update
        .invoke(
            json!({ "name": "calbot", "model": "claude-v2", "grant_id": gid }),
            &admin_ctx,
        )
        .await
        .unwrap();
    assert_eq!(updated["model"], json!("claude-v2"));
    // Replacement, not merge: the earlier tools/channels are gone.
    assert!(updated["tools"].as_array().unwrap().is_empty());
    let after = get
        .invoke(json!({ "name": "calbot" }), &admin_ctx)
        .await
        .unwrap();
    assert_eq!(after["grant_id"], json!(gid));

    // §19 attenuation: a caller whose own authority is narrower than the grant is
    // refused — a profile can never confer more than its creator holds. This caller
    // holds only `agent_profile:write`; the grant reaches `calendar`.
    let weak_ctx = ToolContext {
        workspace_id: Some(ws.id),
        capabilities: Some(vec![Capability::new(
            Action::Write,
            Resource::domain("agent_profile"),
        )]),
        ..Default::default()
    };
    let refused = update
        .invoke(json!({ "name": "calbot", "grant_id": gid }), &weak_ctx)
        .await;
    assert!(matches!(refused, Err(Error::Unauthorized(_))));

    // An unknown grant id is a clear error, not a silent ignore.
    assert!(create
        .invoke(
            json!({ "name": "other", "grant_id": uuid::Uuid::new_v4().to_string() }),
            &admin_ctx,
        )
        .await
        .is_err());

    // Delete removes it; a second delete (and a get) then error.
    let removed = delete
        .invoke(json!({ "name": "calbot" }), &admin_ctx)
        .await
        .unwrap();
    assert_eq!(removed["deleted"], json!("calbot"));
    assert!(get
        .invoke(json!({ "name": "calbot" }), &admin_ctx)
        .await
        .is_err());
    assert!(delete
        .invoke(json!({ "name": "calbot" }), &admin_ctx)
        .await
        .is_err());
}

#[tokio::test]
async fn run_javascript_evaluates_pure_js_over_input() {
    let tool = RunJavascriptTool {
        runner: Arc::new(ScriptCodeRunner::new()),
    };
    let ctx = ToolContext::for_workspace(WorkspaceId::new());

    // `code` is a function body reading the bound `input` and `return`ing.
    let out = tool
        .invoke(
            json!({ "code": "return input.a + input.b;", "input": { "a": 2, "b": 3 } }),
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(out, json!({ "result": 5 }));

    // No `input` → the global is `null`; a body with no `return` → `null`.
    let out = tool
        .invoke(json!({ "code": "var x = 1;" }), &ctx)
        .await
        .unwrap();
    assert_eq!(out, json!({ "result": Json::Null }));

    // The sandbox is pure: fs/net/clock host globals are simply undefined, so
    // reaching for one is a plain JS error surfaced back to the caller.
    let err = tool
        .invoke(json!({ "code": "return require('fs');" }), &ctx)
        .await
        .unwrap_err();
    assert!(!err.to_string().is_empty());

    // A tripped sandbox bound (an infinite loop) fails deterministically
    // rather than hanging the call.
    let err = tool
        .invoke(json!({ "code": "while (true) {}" }), &ctx)
        .await
        .unwrap_err();
    assert!(!err.to_string().is_empty());

    // `code` is required.
    assert!(tool.invoke(json!({ "input": 1 }), &ctx).await.is_err());
}

#[test]
fn run_javascript_is_ungated_like_the_pure_transforms() {
    let tool = RunJavascriptTool {
        runner: Arc::new(ScriptCodeRunner::new()),
    };
    assert_eq!(tool.name(), "run_javascript");
    // The tool confers no authority of its own: with no `callTool` use it is pure
    // compute, and every nested call is capability-gated individually at dispatch.
    assert!(tool.required_capability().is_none());
}

/// A registry holding `run_javascript` plus one ungated and one `notes:write`-gated
/// probe tool — the dispatch surface the nested `catalerum.callTool` tests run over.
fn run_javascript_registry() -> ToolRegistry {
    struct ProbeEchoTool;
    #[async_trait]
    impl Tool for ProbeEchoTool {
        fn name(&self) -> &str {
            "probe_echo"
        }
        fn parameters_schema(&self) -> Json {
            json!({ "type": "object", "properties": {} })
        }
        async fn invoke(&self, args: Json, _ctx: &ToolContext) -> Result<Json> {
            Ok(json!({ "echoed": args }))
        }
    }
    struct ProbeGatedTool;
    #[async_trait]
    impl Tool for ProbeGatedTool {
        fn name(&self) -> &str {
            "probe_gated"
        }
        fn required_capability(&self) -> Option<Capability> {
            Some(Capability::new(Action::Write, Resource::domain("notes")))
        }
        fn parameters_schema(&self) -> Json {
            json!({ "type": "object", "properties": {} })
        }
        async fn invoke(&self, _args: Json, _ctx: &ToolContext) -> Result<Json> {
            Ok(json!({ "ok": true }))
        }
    }
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(RunJavascriptTool {
        runner: Arc::new(ScriptCodeRunner::new()),
    }));
    registry.register(Arc::new(ProbeEchoTool));
    registry.register(Arc::new(ProbeGatedTool));
    registry
}

/// Dispatched through a registry, a `run_javascript` body gets
/// `catalerum.callTool(name, args)` re-dispatching against that SAME registry
/// under the caller's context — the result flows back into the script, and the
/// nested call is capability-checked deny-by-default exactly like a direct call
/// (a denial surfaces as a catchable JS error).
#[tokio::test]
async fn run_javascript_call_tool_redispatches_under_caller_authority() {
    let registry = run_javascript_registry();

    // Caller holds notes:write → both probes reachable; the script chains them.
    let ctx = ToolContext {
        workspace_id: Some(WorkspaceId::new()),
        capabilities: Some(vec![Capability::new(
            Action::Write,
            Resource::domain("notes"),
        )]),
        ..Default::default()
    };
    let out = registry
        .dispatch(
            "run_javascript",
            json!({
                "code": "var e = catalerum.callTool('probe_echo', { n: input.n });\n\
                         var g = catalerum.callTool('probe_gated', {});\n\
                         return { n: e.echoed.n * 2, gated_ok: g.ok };",
                "input": { "n": 21 }
            }),
            &ctx,
        )
        .await
        .expect("script with allowed nested calls runs");
    assert_eq!(out, json!({ "result": { "n": 42, "gated_ok": true } }));

    // An empty cap set reaches the ungated probe but is DENIED the gated one —
    // and the denial is a catchable JS error, not a silent pass.
    let confined = ToolContext {
        workspace_id: Some(WorkspaceId::new()),
        capabilities: Some(Vec::new()),
        ..Default::default()
    };
    let out = registry
        .dispatch(
            "run_javascript",
            json!({
                "code": "try { catalerum.callTool('probe_gated', {}); return 'ran'; }\n\
                         catch (e) { return 'denied'; }"
            }),
            &confined,
        )
        .await
        .expect("script survives a denied nested call via catch");
    assert_eq!(out["result"], json!("denied"), "gated tool must not run");
}

/// The nested bridge fails closed where it must: `run_javascript` refuses to call
/// itself (no Boa-in-Boa recursion), and a direct `invoke` without a dispatching
/// registry keeps the sandbox pure (`catalerum` undefined → catchable error).
#[tokio::test]
async fn run_javascript_nested_bridge_fails_closed() {
    let registry = run_javascript_registry();
    let ctx = ToolContext {
        workspace_id: Some(WorkspaceId::new()),
        capabilities: Some(Vec::new()),
        ..Default::default()
    };

    // Self-recursion is refused by the host (catchable in-script).
    let out = registry
        .dispatch(
            "run_javascript",
            json!({
                "code": "try { catalerum.callTool('run_javascript', { code: 'return 1;' }); \
                         return 'recursed'; } catch (e) { return 'refused'; }"
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(out["result"], json!("refused"));

    // A UI-handler context (`ui_id` set) must not tunnel past the `[ui].handler_tools`
    // allow-list: the eval stays pure, so `catalerum` is undefined.
    let ui_ctx = ToolContext {
        ui_id: Some(UiDefinitionId::new()),
        ..ctx.clone()
    };
    let out = registry
        .dispatch(
            "run_javascript",
            json!({
                "code": "try { catalerum.callTool('probe_echo', {}); return 'tunnelled'; } \
                         catch (e) { return 'pure'; }"
            }),
            &ui_ctx,
        )
        .await
        .unwrap();
    assert_eq!(out["result"], json!("pure"));

    // Invoked directly (no dispatch → no registry in the context), the sandbox is
    // the pure transform it always was.
    let tool = RunJavascriptTool {
        runner: Arc::new(ScriptCodeRunner::new()),
    };
    let out = tool
        .invoke(
            json!({ "code": "try { catalerum.callTool('probe_echo', {}); return 'reached'; } \
                             catch (e) { return 'pure'; }" }),
            &ToolContext::for_workspace(WorkspaceId::new()),
        )
        .await
        .unwrap();
    assert_eq!(out["result"], json!("pure"));
}

// --- per-App durable key/value store (SOUL §12/§29) ----------------------

/// Register the four `app_data` tools against a fresh registry (the shape the
/// UI runtime + chat dispatch against).
fn app_data_registry(store: &Store) -> ToolRegistry {
    let mut r = ToolRegistry::new();
    r.register(Arc::new(AppDataGetTool {
        store: store.clone(),
    }));
    r.register(Arc::new(AppDataSetTool {
        store: store.clone(),
    }));
    r.register(Arc::new(AppDataListTool {
        store: store.clone(),
    }));
    r.register(Arc::new(AppDataDeleteTool {
        store: store.clone(),
    }));
    r
}

/// Repo-level CRUD plus the value-size and per-App key caps (SOUL §12/§29).
#[tokio::test]
async fn app_data_repo_crud_and_caps() {
    let Some(url) = db_url() else {
        eprintln!("skipping app_data repo test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    let store = Store::connect(&url).await.expect("store");
    let ws = store
        .workspaces()
        .create("appdata", &format!("appdata-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let repo = store.app_data();
    let app = "app-1";

    // Unset → None; set → stored; get → the same value; overwrite → replaced.
    assert!(repo.get(ws.id, app, "k").await.unwrap().is_none());
    let e = repo.set(ws.id, app, "k", &json!({ "n": 1 })).await.unwrap();
    assert_eq!(e.value, json!({ "n": 1 }));
    assert_eq!(
        repo.get(ws.id, app, "k").await.unwrap().unwrap().value,
        json!({ "n": 1 })
    );
    repo.set(ws.id, app, "k", &json!({ "n": 2 })).await.unwrap();
    assert_eq!(
        repo.get(ws.id, app, "k").await.unwrap().unwrap().value,
        json!({ "n": 2 })
    );

    // list + count reflect the single key; delete is idempotent.
    repo.set(ws.id, app, "k2", &json!("v")).await.unwrap();
    let listed = repo.list(ws.id, app, 100).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(repo.count(ws.id, app).await.unwrap(), 2);
    assert!(repo.delete(ws.id, app, "k").await.unwrap());
    assert!(!repo.delete(ws.id, app, "k").await.unwrap()); // gone → false
    assert_eq!(repo.count(ws.id, app).await.unwrap(), 1);

    // Blank app/key rejected.
    assert!(repo.get(ws.id, "  ", "k").await.is_err());
    assert!(repo.set(ws.id, app, "  ", &json!(1)).await.is_err());

    // Value-size cap: a value over MAX_APP_DATA_VALUE_BYTES is rejected.
    let huge = json!("x".repeat(catalerum_store::MAX_APP_DATA_VALUE_BYTES + 1));
    assert!(repo.set(ws.id, app, "big", &huge).await.is_err());
}

/// The tools gate on `ui:read` (get/list) and `ui:write` (set/delete): a Viewer
/// reads but cannot write; a Member does both (SOUL §12/§19).
#[tokio::test]
async fn app_data_tools_gate_on_ui_capabilities() {
    use catalerum_core::model::Role;
    let Some(url) = db_url() else {
        eprintln!("skipping app_data gating test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    let store = Store::connect(&url).await.expect("store");
    let ws = store
        .workspaces()
        .create(
            "appdatagate",
            &format!("appdatagate-{}", uuid::Uuid::new_v4()),
        )
        .await
        .expect("ws");
    let registry = app_data_registry(&store);

    let member = ToolContext {
        workspace_id: Some(ws.id),
        capabilities: Some(catalerum_iam::base_capabilities(Role::Member)),
        ..Default::default()
    };
    let viewer = ToolContext {
        workspace_id: Some(ws.id),
        capabilities: Some(catalerum_iam::base_capabilities(Role::Viewer)),
        ..Default::default()
    };

    // Member writes + reads (namespace named explicitly — no ui_id here).
    registry
        .dispatch(
            "app_data_set",
            json!({ "app": "a", "key": "k", "value": 1 }),
            &member,
        )
        .await
        .expect("member set");
    let got = registry
        .dispatch("app_data_get", json!({ "app": "a", "key": "k" }), &member)
        .await
        .expect("member get");
    assert_eq!(got["value"], json!(1));

    // Viewer may read (ui:read) …
    let got = registry
        .dispatch("app_data_get", json!({ "app": "a", "key": "k" }), &viewer)
        .await
        .expect("viewer get");
    assert_eq!(got["found"], json!(true));
    // … but not write / delete (ui:write) — deny-by-default.
    assert!(registry
        .dispatch(
            "app_data_set",
            json!({ "app": "a", "key": "k2", "value": 2 }),
            &viewer
        )
        .await
        .is_err());
    assert!(registry
        .dispatch(
            "app_data_delete",
            json!({ "app": "a", "key": "k" }),
            &viewer
        )
        .await
        .is_err());

    // No ui_id and no `app` argument → a clear caller error (namespace required).
    assert!(registry
        .dispatch("app_data_get", json!({ "key": "k" }), &member)
        .await
        .is_err());
}

/// From an App handler the namespace is forced to the firing App's id: a caller
/// `app` argument is ignored, and one App cannot read another App's keys
/// (SOUL §12/§29 isolation).
#[tokio::test]
async fn app_data_handler_forces_namespace_and_isolates_apps() {
    use catalerum_core::model::Role;
    let Some(url) = db_url() else {
        eprintln!(
            "skipping app_data isolation test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
        );
        return;
    };
    let store = Store::connect(&url).await.expect("store");
    let ws = store
        .workspaces()
        .create(
            "appdataiso",
            &format!("appdataiso-{}", uuid::Uuid::new_v4()),
        )
        .await
        .expect("ws");
    let registry = app_data_registry(&store);

    let app_a = UiDefinitionId::new();
    let app_b = UiDefinitionId::new();
    let caps = Some(catalerum_iam::base_capabilities(Role::Member));
    let ctx_a = ToolContext {
        workspace_id: Some(ws.id),
        capabilities: caps.clone(),
        ui_id: Some(app_a),
        ..Default::default()
    };
    let ctx_b = ToolContext {
        workspace_id: Some(ws.id),
        capabilities: caps,
        ui_id: Some(app_b),
        ..Default::default()
    };

    // App A writes "secret" — and tries to spoof `app: <B>`, which is IGNORED
    // (the handler namespace is forced to A).
    let set = registry
        .dispatch(
            "app_data_set",
            json!({ "key": "secret", "value": "A-only", "app": app_b.to_string() }),
            &ctx_a,
        )
        .await
        .expect("A set");
    assert_eq!(set["app"], json!(app_a.to_string()));

    // App A reads its own key.
    let got_a = registry
        .dispatch("app_data_get", json!({ "key": "secret" }), &ctx_a)
        .await
        .expect("A get");
    assert_eq!(got_a["found"], json!(true));
    assert_eq!(got_a["value"], json!("A-only"));

    // App B cannot see A's key (namespace forced to B), and its list is empty —
    // proving the spoofed `app: <B>` above did NOT leak into B.
    let got_b = registry
        .dispatch("app_data_get", json!({ "key": "secret" }), &ctx_b)
        .await
        .expect("B get");
    assert_eq!(got_b["found"], json!(false));
    let list_b = registry
        .dispatch("app_data_list", json!({}), &ctx_b)
        .await
        .expect("B list");
    assert_eq!(list_b["count"], json!(0));
}

/// A shell suite (shell `app_ref`s the sub-app, sub-app names the shell via
/// `parent_app` — mutual, server-verified opt-in) shares one durable
/// namespace: the shell's. A one-sided claim (parent_app without the
/// reciprocal `app_ref`) stays isolated (SOUL §12/§29).
#[tokio::test]
async fn app_data_shell_suite_shares_namespace() {
    use catalerum_core::model::Role;
    let Some(url) = db_url() else {
        eprintln!("skipping app_data shell test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    let store = Store::connect(&url).await.expect("store");
    let ws = store
        .workspaces()
        .create(
            "appdatashell",
            &format!("appdatashell-{}", uuid::Uuid::new_v4()),
        )
        .await
        .expect("ws");
    let registry = app_data_registry(&store);
    let author = Author::User { id: UserId::new() };

    let spec = |json: Json| -> catalerum_core::model_ui::UiSpec {
        serde_json::from_value(json).expect("spec")
    };
    let leaf_view = |root: Json| json!({ "default_view": "v", "views": [{ "id": "v", "title": "V", "root": root }] });

    // Sub-app first (no parent yet), then the shell embedding it, then the
    // sub-app's parent_app back-reference — the required authoring order.
    let sub = store
        .ui_definitions()
        .create(
            ws.id,
            author,
            UI_SPEC_VERSION,
            &UiDefinitionInput {
                name: Some("sub".into()),
                title: "Sub".into(),
                description: None,
                definition: spec(leaf_view(json!({ "id": "r", "kind": "stack" }))),
            },
        )
        .await
        .expect("sub");
    let shell = store
        .ui_definitions()
        .create(
            ws.id,
            author,
            UI_SPEC_VERSION,
            &UiDefinitionInput {
                name: Some("shell".into()),
                title: "Shell".into(),
                description: None,
                definition: spec(leaf_view(json!({
                    "id": "r", "kind": "stack", "children": [
                        { "id": "sub", "kind": "app_ref", "props": { "app": sub.id.to_string() } }
                    ]
                }))),
            },
        )
        .await
        .expect("shell");
    let mut sub_spec = leaf_view(json!({ "id": "r", "kind": "stack" }));
    sub_spec["parent_app"] = json!(shell.id.to_string());
    store
        .ui_definitions()
        .update_definition(
            ws.id,
            sub.id,
            sub.version,
            &UiDefinitionInput {
                name: Some("sub".into()),
                title: "Sub".into(),
                description: None,
                definition: spec(sub_spec),
            },
        )
        .await
        .expect("sub update");

    let caps = Some(catalerum_iam::base_capabilities(Role::Member));
    let ctx_shell = ToolContext {
        workspace_id: Some(ws.id),
        capabilities: caps.clone(),
        ui_id: Some(shell.id),
        ..Default::default()
    };
    let ctx_sub = ToolContext {
        workspace_id: Some(ws.id),
        capabilities: caps.clone(),
        ui_id: Some(sub.id),
        ..Default::default()
    };

    // The shell writes; the sub-app reads the SAME row — both resolve to the
    // shell's namespace.
    let set = registry
        .dispatch(
            "app_data_set",
            json!({ "key": "recipes", "value": ["cake"] }),
            &ctx_shell,
        )
        .await
        .expect("shell set");
    assert_eq!(set["app"], json!(shell.id.to_string()));
    let got = registry
        .dispatch("app_data_get", json!({ "key": "recipes" }), &ctx_sub)
        .await
        .expect("sub get");
    assert_eq!(got["found"], json!(true));
    assert_eq!(got["value"], json!(["cake"]));
    assert_eq!(got["app"], json!(shell.id.to_string()));

    // One-sided claim: an app naming the shell as parent WITHOUT the shell
    // `app_ref`-embedding it stays in its own namespace.
    let mut rogue_spec = leaf_view(json!({ "id": "r", "kind": "stack" }));
    rogue_spec["parent_app"] = json!(shell.id.to_string());
    let rogue = store
        .ui_definitions()
        .create(
            ws.id,
            author,
            UI_SPEC_VERSION,
            &UiDefinitionInput {
                name: Some("rogue".into()),
                title: "Rogue".into(),
                description: None,
                definition: spec(rogue_spec),
            },
        )
        .await
        .expect("rogue");
    let ctx_rogue = ToolContext {
        workspace_id: Some(ws.id),
        capabilities: caps,
        ui_id: Some(rogue.id),
        ..Default::default()
    };
    let got = registry
        .dispatch("app_data_get", json!({ "key": "recipes" }), &ctx_rogue)
        .await
        .expect("rogue get");
    assert_eq!(
        got["found"],
        json!(false),
        "one-sided parent claim must not share"
    );
    assert_eq!(got["app"], json!(rogue.id.to_string()));

    // A shell may also embed its sub-app by NAME (the `app_ref` name form):
    // the reciprocal check matches the child's name slug, so the suite still
    // shares the shell's namespace.
    let author2 = Author::User { id: UserId::new() };
    let sub2 = store
        .ui_definitions()
        .create(
            ws.id,
            author2,
            UI_SPEC_VERSION,
            &UiDefinitionInput {
                name: Some("recipes-editor".into()),
                title: "Editor".into(),
                description: None,
                definition: spec(leaf_view(json!({ "id": "r", "kind": "stack" }))),
            },
        )
        .await
        .expect("sub2");
    let shell2 = store
        .ui_definitions()
        .create(
            ws.id,
            author2,
            UI_SPEC_VERSION,
            &UiDefinitionInput {
                name: Some("shell2".into()),
                title: "Shell2".into(),
                description: None,
                definition: spec(leaf_view(json!({
                    "id": "r", "kind": "stack", "children": [
                        { "id": "sub", "kind": "app_ref", "props": { "app": "recipes-editor" } }
                    ]
                }))),
            },
        )
        .await
        .expect("shell2");
    let mut sub2_spec = leaf_view(json!({ "id": "r", "kind": "stack" }));
    sub2_spec["parent_app"] = json!(shell2.id.to_string());
    store
        .ui_definitions()
        .update_definition(
            ws.id,
            sub2.id,
            sub2.version,
            &UiDefinitionInput {
                name: Some("recipes-editor".into()),
                title: "Editor".into(),
                description: None,
                definition: spec(sub2_spec),
            },
        )
        .await
        .expect("sub2 update");
    let ctx_sub2 = ToolContext {
        workspace_id: Some(ws.id),
        capabilities: Some(catalerum_iam::base_capabilities(Role::Member)),
        ui_id: Some(sub2.id),
        ..Default::default()
    };
    let set = registry
        .dispatch(
            "app_data_set",
            json!({ "key": "draft", "value": "x" }),
            &ctx_sub2,
        )
        .await
        .expect("sub2 set");
    assert_eq!(
        set["app"],
        json!(shell2.id.to_string()),
        "name-form app_ref must still share the shell namespace"
    );
}
