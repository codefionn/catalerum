//! Integration test: the `AgentProfileRepo` contract (SOUL §19/§25, §6.1/§18).
//! Create / get / get_by_name / list / idempotent upsert-by-name / delete, the
//! channel-routing lookup (`list_by_channel`), the same-workspace grant FK
//! (including `ON DELETE SET NULL`), and cross-workspace isolation.
//!
//! Same DB gating as the other store tests: set `CATALERUM_TEST_DATABASE_URL`
//! (or `DATABASE_URL`) to run it; otherwise it skips and passes offline.

use catalerum_core::capability::{Action, Capability, Constraints, Resource};
use catalerum_core::model::{GuardFail, ObjectLabelPolicy, Origin, ToolGuard, ToolGuardLlm};
use catalerum_store::{NewAgentProfile, Store, StoreError};

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

fn profile(name: &str) -> NewAgentProfile {
    NewAgentProfile {
        name: name.to_string(),
        model: Some("anthropic/claude-opus-4-8".into()),
        system_prompt: Some("You are a calendar bot.".into()),
        tools: vec!["get_events".into(), "notify".into()],
        skills: vec!["weekly-review".into()],
        subagents: vec![],
        channels: vec!["discord".into()],
        grant_id: None,
        guard: None,
    }
}

#[tokio::test]
async fn agent_profile_crud_channel_routing_grant_fk_and_isolation() {
    let Some(url) = test_db_url() else {
        eprintln!("skipping agent_profile_crud_…: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };

    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("ap", &format!("ap-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let other = store
        .workspaces()
        .create("ap-b", &format!("ap-b-{}", uuid::Uuid::new_v4()))
        .await
        .expect("other ws");

    // Create + read back by id and by name; the name lists round-trip.
    let created = store
        .agent_profiles()
        .create(ws.id, &profile("calbot"))
        .await
        .expect("create");
    assert_eq!(created.model.as_deref(), Some("anthropic/claude-opus-4-8"));
    assert_eq!(
        created.tools,
        vec!["get_events".to_string(), "notify".to_string()]
    );
    assert_eq!(created.channels, vec!["discord".to_string()]);
    assert!(created.grant_id.is_none());
    assert!(
        created.guard.is_none(),
        "a profile with no guard reads back None"
    );
    assert_eq!(
        store
            .agent_profiles()
            .get(ws.id, created.id)
            .await
            .unwrap()
            .name,
        "calbot"
    );
    assert_eq!(
        store
            .agent_profiles()
            .get_by_name(ws.id, "calbot")
            .await
            .unwrap()
            .unwrap()
            .id,
        created.id
    );
    assert!(store
        .agent_profiles()
        .get_by_name(ws.id, "nope")
        .await
        .unwrap()
        .is_none());

    // Duplicate name → conflict.
    assert!(matches!(
        store
            .agent_profiles()
            .create(ws.id, &profile("calbot"))
            .await,
        Err(StoreError::Conflict(_))
    ));

    // Channel-routing lookup: a profile listening on `discord` is returned for
    // `discord`, not for a channel it doesn't list.
    let on_discord = store
        .agent_profiles()
        .list_by_channel(ws.id, "discord")
        .await
        .unwrap();
    assert_eq!(on_discord.len(), 1);
    assert_eq!(on_discord[0].id, created.id);
    assert!(store
        .agent_profiles()
        .list_by_channel(ws.id, "telegram")
        .await
        .unwrap()
        .is_empty());

    // Upsert-by-name refreshes every column in DO UPDATE SET, keeping the id —
    // including the riskiest paths: model None, a different channel set, subagents.
    let mut updated = profile("calbot");
    updated.model = None; // None overwrite (Some -> None)
    updated.system_prompt = None;
    updated.tools = vec!["query_graph".into()];
    updated.skills = vec![];
    updated.subagents = vec!["researcher".into()];
    updated.channels = vec!["telegram".into()];
    // Attach a full tool guard (Boa script + declarative LLM + fail-open) so the new
    // JSONB column round-trips through the upsert path.
    updated.guard = Some(ToolGuard {
        script: Some(
            "return input.capability && input.capability.read_only ? 'allow' : 'ask';".into(),
        ),
        llm: Some(ToolGuardLlm {
            model: Some("anthropic/claude-haiku-4-5".into()),
            instruction: "Deny anything that writes to production.".into(),
        }),
        object_labels: Some(ObjectLabelPolicy {
            require_any: vec!["shared".into()],
            deny: vec!["confidential".into()],
        }),
        on_error: GuardFail::Allow,
    });
    let up = store
        .agent_profiles()
        .upsert_by_name(ws.id, &updated)
        .await
        .expect("upsert");
    assert_eq!(up.id, created.id, "upsert keeps the stable id");
    assert!(up.model.is_none(), "model = EXCLUDED clears it to None");
    assert_eq!(up.tools, vec!["query_graph".to_string()]);
    assert_eq!(up.subagents, vec!["researcher".to_string()]);
    assert_eq!(up.channels, vec!["telegram".to_string()]);
    assert_eq!(up.guard, updated.guard, "the guard JSONB round-trips");
    // And it survives a fresh read (not just the RETURNING row).
    let reread = store.agent_profiles().get(ws.id, created.id).await.unwrap();
    assert_eq!(reread.guard, updated.guard);
    // Routing now follows the new channel set.
    assert!(store
        .agent_profiles()
        .list_by_channel(ws.id, "discord")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        store
            .agent_profiles()
            .list_by_channel(ws.id, "telegram")
            .await
            .unwrap()
            .len(),
        1
    );

    // Same-workspace grant FK: a profile may reference a grant in its own
    // workspace, and the id round-trips.
    let grant = store
        .grants()
        .upsert(
            ws.id,
            "calbot-grant",
            &[Capability::new(Action::Read, Resource::domain("calendar"))],
            &Constraints::default(),
        )
        .await
        .expect("grant");
    let mut granted = profile("granted-bot");
    granted.grant_id = Some(grant.id);
    let g = store
        .agent_profiles()
        .create(ws.id, &granted)
        .await
        .expect("create granted");
    assert_eq!(g.grant_id, Some(grant.id), "grant_id round-trips");

    // ON DELETE SET NULL: deleting the grant detaches the profile (grant_id -> NULL)
    // rather than dangling or cascading the profile away.
    store
        .grants()
        .delete(ws.id, grant.id)
        .await
        .expect("del grant");
    let after = store.agent_profiles().get(ws.id, g.id).await.unwrap();
    assert!(
        after.grant_id.is_none(),
        "grant delete nulls the profile's grant_id, leaving the profile intact"
    );

    // Cross-workspace isolation: another workspace sees none of these.
    assert!(store
        .agent_profiles()
        .list_by_workspace(other.id)
        .await
        .unwrap()
        .is_empty());
    assert!(store
        .agent_profiles()
        .get_by_name(other.id, "calbot")
        .await
        .unwrap()
        .is_none());
    assert!(matches!(
        store.agent_profiles().get(other.id, created.id).await,
        Err(StoreError::NotFound)
    ));
    assert!(store
        .agent_profiles()
        .list_by_channel(other.id, "telegram")
        .await
        .unwrap()
        .is_empty());

    // A delete scoped to the wrong workspace is a no-op (NotFound) and leaves the
    // real row intact — tenant isolation on the destructive path (§18).
    assert!(matches!(
        store.agent_profiles().delete(other.id, created.id).await,
        Err(StoreError::NotFound)
    ));
    assert!(store.agent_profiles().get(ws.id, created.id).await.is_ok());

    // Delete; gone afterwards, and a repeat delete is NotFound.
    store
        .agent_profiles()
        .delete(ws.id, created.id)
        .await
        .unwrap();
    assert!(matches!(
        store.agent_profiles().get(ws.id, created.id).await,
        Err(StoreError::NotFound)
    ));
    assert!(matches!(
        store.agent_profiles().delete(ws.id, created.id).await,
        Err(StoreError::NotFound)
    ));
}

#[tokio::test]
async fn conversation_profile_binding_and_detach_on_delete() {
    let Some(url) = test_db_url() else {
        eprintln!("skipping conversation_profile_binding_…: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("cap", &format!("cap-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let other = store
        .workspaces()
        .create("cap-b", &format!("cap-b-{}", uuid::Uuid::new_v4()))
        .await
        .expect("other ws");

    let profile = store
        .agent_profiles()
        .create(ws.id, &profile("calbot"))
        .await
        .expect("profile");
    let conv = store
        .conversations()
        .create(ws.id, Some("chat"), Origin::Web)
        .await
        .expect("conversation");
    assert!(conv.agent_profile_id.is_none(), "starts unbound");

    // Bind the conversation to the profile (the chat picker), read it back.
    let bound = store
        .conversations()
        .set_agent_profile(ws.id, conv.id, Some(profile.id))
        .await
        .expect("bind");
    assert_eq!(bound.agent_profile_id, Some(profile.id));
    assert_eq!(
        store
            .conversations()
            .get(ws.id, conv.id)
            .await
            .unwrap()
            .agent_profile_id,
        Some(profile.id),
        "binding persists"
    );

    // Deleting the profile detaches the conversation (FK ON DELETE SET NULL) —
    // never cascades the conversation away.
    store
        .agent_profiles()
        .delete(ws.id, profile.id)
        .await
        .expect("delete profile");
    assert!(
        store
            .conversations()
            .get(ws.id, conv.id)
            .await
            .unwrap()
            .agent_profile_id
            .is_none(),
        "profile delete nulls the conversation's agent_profile_id"
    );

    // Unbind path (None) is a no-op-but-valid update; cross-workspace bind is a
    // NotFound (tenant isolation on the destructive/mutating path, §18).
    store
        .conversations()
        .set_agent_profile(ws.id, conv.id, None)
        .await
        .expect("unbind");
    assert!(matches!(
        store
            .conversations()
            .set_agent_profile(other.id, conv.id, None)
            .await,
        Err(StoreError::NotFound)
    ));
}
