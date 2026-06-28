//! Integration test: the `McpServerRepo` contract (SOUL §26, §6.1/§18). Create /
//! get_by_name / list / idempotent upsert-by-name / delete-by-name, the
//! boot-time `list_enabled` (cross-workspace, enabled-only), and cross-workspace
//! isolation.
//!
//! Same DB gating as the other store tests: set `CATALERUM_TEST_DATABASE_URL` (or
//! `DATABASE_URL`) to run it; otherwise it skips and passes offline.

use std::collections::BTreeMap;

use catalerum_core::model::McpAuthSpec;
use catalerum_store::{NewMcpServerDef, Store, StoreError};

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

fn stdio(name: &str) -> NewMcpServerDef {
    NewMcpServerDef {
        name: name.to_string(),
        transport: "stdio".into(),
        command: "npx".into(),
        args: vec!["@playwright/mcp".into(), "--headless".into()],
        env: BTreeMap::from([("DISPLAY".to_string(), ":0".to_string())]),
        url: String::new(),
        auth: McpAuthSpec::default(),
        enabled: true,
        tools: vec!["browser_navigate".into()],
    }
}

#[tokio::test]
async fn mcp_server_crud_upsert_list_enabled_and_isolation() {
    let Some(url) = test_db_url() else {
        eprintln!("skipping mcp_server_crud_…: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };

    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("mcp", &format!("mcp-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let other = store
        .workspaces()
        .create("mcp-b", &format!("mcp-b-{}", uuid::Uuid::new_v4()))
        .await
        .expect("other ws");

    // Create + read back by name; JSONB args/env/tools round-trip.
    let created = store
        .mcp_servers()
        .create(ws.id, &stdio("pw"))
        .await
        .expect("create");
    assert_eq!(created.transport, "stdio");
    assert_eq!(created.command, "npx");
    assert_eq!(
        created.args,
        vec!["@playwright/mcp".to_string(), "--headless".to_string()]
    );
    assert_eq!(created.env.get("DISPLAY").map(String::as_str), Some(":0"));
    assert_eq!(created.tools, vec!["browser_navigate".to_string()]);
    assert!(created.enabled);
    assert_eq!(
        store
            .mcp_servers()
            .get_by_name(ws.id, "pw")
            .await
            .unwrap()
            .unwrap()
            .id,
        created.id
    );
    assert!(store
        .mcp_servers()
        .get_by_name(ws.id, "nope")
        .await
        .unwrap()
        .is_none());

    // Duplicate name → conflict.
    assert!(matches!(
        store.mcp_servers().create(ws.id, &stdio("pw")).await,
        Err(StoreError::Conflict(_))
    ));

    // Upsert-by-name replaces every column (here: switch to an http transport with
    // bearer auth, and disable it), keeping the stable id.
    let mut edited = stdio("pw");
    edited.transport = "http".into();
    edited.command = String::new();
    edited.args = vec![];
    edited.env = BTreeMap::new();
    edited.url = "https://acme/mcp".into();
    edited.auth = McpAuthSpec {
        kind: "bearer".into(),
        token: "sekret".into(),
        ..Default::default()
    };
    edited.enabled = false;
    let up = store
        .mcp_servers()
        .upsert_by_name(ws.id, &edited)
        .await
        .expect("upsert");
    assert_eq!(up.id, created.id, "upsert keeps the stable id");
    assert_eq!(up.transport, "http");
    assert_eq!(up.url, "https://acme/mcp");
    assert_eq!(up.auth.kind, "bearer");
    assert_eq!(
        up.auth.token, "sekret",
        "auth secret round-trips through JSONB"
    );
    assert!(!up.enabled);

    // `list_enabled` is cross-workspace and enabled-only: our now-disabled server
    // is excluded. Add an enabled one in the *other* workspace and confirm it's
    // listed (the boot loader reconnects across workspaces).
    assert!(
        store
            .mcp_servers()
            .list_enabled()
            .await
            .unwrap()
            .iter()
            .all(|s| s.name != "pw"),
        "disabled server is excluded from list_enabled"
    );
    let other_srv = store
        .mcp_servers()
        .create(other.id, &stdio("fs"))
        .await
        .expect("other create");
    assert!(
        store
            .mcp_servers()
            .list_enabled()
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == other_srv.id),
        "an enabled server in another workspace is listed for boot reconnect"
    );

    // Cross-workspace isolation: the other workspace's by-name/list views don't
    // see ws's server, and a wrong-workspace delete is a no-op (NotFound).
    assert!(store
        .mcp_servers()
        .get_by_name(other.id, "pw")
        .await
        .unwrap()
        .is_none());
    assert!(store
        .mcp_servers()
        .list_by_workspace(other.id)
        .await
        .unwrap()
        .iter()
        .all(|s| s.name != "pw"));
    assert!(matches!(
        store.mcp_servers().delete_by_name(other.id, "pw").await,
        Err(StoreError::NotFound)
    ));
    assert!(store
        .mcp_servers()
        .get_by_name(ws.id, "pw")
        .await
        .unwrap()
        .is_some());

    // Delete; gone afterwards, and a repeat delete is NotFound.
    store
        .mcp_servers()
        .delete_by_name(ws.id, "pw")
        .await
        .unwrap();
    assert!(store
        .mcp_servers()
        .get_by_name(ws.id, "pw")
        .await
        .unwrap()
        .is_none());
    assert!(matches!(
        store.mcp_servers().delete_by_name(ws.id, "pw").await,
        Err(StoreError::NotFound)
    ));
}
