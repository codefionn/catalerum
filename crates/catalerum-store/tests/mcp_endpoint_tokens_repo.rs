//! Integration test: `McpEndpointTokenRepo` — the revocable, hash-only record
//! behind `POST /mcp/s/{token}` (SOUL §26): mint → live lookup → revoke → dead,
//! plus workspace scoping and the endpoint-delete cascade.
//!
//! DB-gated like the other store tests: set `CATALERUM_TEST_DATABASE_URL` (or
//! `DATABASE_URL`) to run it; otherwise it skips and passes offline.

use catalerum_core::model::Author;
use catalerum_core::UserId;
use catalerum_store::{McpEndpointInput, Store, StoreError};

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

fn endpoint_input(name: &str) -> McpEndpointInput {
    McpEndpointInput {
        name: name.to_string(),
        description: String::new(),
        script: "// test".to_string(),
        bucket_name: None,
        key_prefix: None,
        grant_id: None,
        enabled: true,
    }
}

#[tokio::test]
async fn endpoint_tokens_are_revocable_and_workspace_scoped() {
    let Some(url) = test_db_url() else {
        eprintln!("skipping endpoint_tokens_are_revocable_and_workspace_scoped: set CATALERUM_TEST_DATABASE_URL");
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("met", &format!("met-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let other = store
        .workspaces()
        .create("met2", &format!("met2-{}", uuid::Uuid::new_v4()))
        .await
        .expect("other ws");
    let endpoint = store
        .mcp_endpoints()
        .create(
            ws.id,
            Author::User {
                id: UserId::new(),
            },
            &endpoint_input("wiki"),
        )
        .await
        .expect("endpoint");

    let expires = chrono::Utc::now() + chrono::Duration::days(30);
    let minted = store
        .mcp_endpoint_tokens()
        .create(ws.id, endpoint.id, "hash-of-token-a", expires)
        .await
        .expect("mint");

    // Live lookup finds it; the projection never exposes the hash.
    let live = store
        .mcp_endpoint_tokens()
        .get_live_by_token_hash("hash-of-token-a")
        .await
        .expect("live");
    assert_eq!(live.id, minted.id);
    assert_eq!(live.workspace_id, ws.id);
    assert_eq!(live.endpoint_id, endpoint.id);
    assert!(live.revoked_at.is_none());

    // The management listing shows it, scoped to the endpoint.
    let listed = store
        .mcp_endpoint_tokens()
        .list_by_endpoint(ws.id, endpoint.id)
        .await
        .expect("list");
    assert_eq!(listed.len(), 1);
    assert!(
        store
            .mcp_endpoint_tokens()
            .list_by_endpoint(other.id, endpoint.id)
            .await
            .expect("list other")
            .is_empty(),
        "a foreign workspace lists nothing"
    );

    // Revoke: the live lookup immediately stops matching; re-revoke is
    // idempotent; revoking a random id is NotFound.
    store
        .mcp_endpoint_tokens()
        .revoke(ws.id, endpoint.id, minted.id)
        .await
        .expect("revoke");
    assert!(matches!(
        store
            .mcp_endpoint_tokens()
            .get_live_by_token_hash("hash-of-token-a")
            .await,
        Err(StoreError::NotFound)
    ),
        "a revoked token is no longer live"
    );
    store
        .mcp_endpoint_tokens()
        .revoke(ws.id, endpoint.id, minted.id)
        .await
        .expect("re-revoke is idempotent");
    assert!(matches!(
        store
            .mcp_endpoint_tokens()
            .revoke(ws.id, endpoint.id, uuid::Uuid::new_v4())
            .await,
        Err(StoreError::NotFound)
    ),
        "an unknown token id 404s"
    );
    assert!(matches!(
        store
            .mcp_endpoint_tokens()
            .revoke(other.id, endpoint.id, minted.id)
            .await,
        Err(StoreError::NotFound)
    ),
        "a foreign workspace cannot revoke"
    );

    // An already-expired token is never live.
    let past = chrono::Utc::now() - chrono::Duration::hours(1);
    store
        .mcp_endpoint_tokens()
        .create(ws.id, endpoint.id, "hash-of-token-expired", past)
        .await
        .expect("mint expired");
    assert!(matches!(
        store
            .mcp_endpoint_tokens()
            .get_live_by_token_hash("hash-of-token-expired")
            .await,
        Err(StoreError::NotFound)
    ),
        "an expired token is not live"
    );

    // Deleting the endpoint cascades its tokens away.
    store
        .mcp_endpoint_tokens()
        .create(ws.id, endpoint.id, "hash-of-token-b", expires)
        .await
        .expect("mint b");
    store
        .mcp_endpoints()
        .delete(ws.id, endpoint.id)
        .await
        .expect("delete endpoint");
    assert!(matches!(
        store
            .mcp_endpoint_tokens()
            .get_live_by_token_hash("hash-of-token-b")
            .await,
        Err(StoreError::NotFound)
    ),
        "deleting the endpoint kills its tokens"
    );
    assert!(store
        .mcp_endpoint_tokens()
        .list_by_endpoint(ws.id, endpoint.id)
        .await
        .expect("list after cascade")
        .is_empty());
}
