//! Authenticated native decision calls.

use axum::{extract::State, routing::post, Json, Router};
use catalerum_core::capability::Action;
use catalerum_llm::DecisionRequest;
use serde_json::{json, Value};

use crate::tools::{
    check_decision_cost_limit, decision_model, decision_output, validate_decision_model,
    DecisionInput,
};
use crate::{
    auth::Auth,
    error::{ApiError, ApiResult},
    state::AppState,
};

pub fn router() -> Router<AppState> {
    Router::new().route("/decisions", post(decide))
}

async fn decide(
    State(state): State<AppState>,
    auth: Auth,
    Json(input): Json<DecisionInput>,
) -> ApiResult<Json<Value>> {
    auth.require(Action::Write, "conversation")?;
    input.validate()?;
    let principal = auth.principal();
    let settings = if input.model.is_none() {
        state
            .store()
            .llm_settings()
            .get(principal.workspace_id, principal.user_id)
            .await
            .ok()
    } else {
        None
    };
    let model = decision_model(
        input.model.as_deref(),
        settings.as_ref().and_then(|s| s.decision_model.as_deref()),
        state.config().llm.decision_model.as_deref(),
    )?;
    if auth.grant().is_some_and(|grant| grant.constraints.dry_run) {
        return Ok(Json(json!({"dry_run": true, "model": model})));
    }
    check_decision_cost_limit(auth.grant().and_then(|grant| grant.constraints.cost_limit))
        .map_err(|err| ApiError::Forbidden(err.to_string()))?;
    validate_decision_model(state.llm(), &model).await?;
    let response = state
        .llm()
        .decide(DecisionRequest {
            model,
            state: input.state,
            questions: input.questions,
        })
        .await?;
    Ok(Json(decision_output(response)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use catalerum_core::{model::Role, UserId, WorkspaceId};
    use catalerum_iam::Principal;

    #[test]
    fn decision_spending_requires_conversation_write() {
        let principal = |role| Principal::new(UserId::new(), WorkspaceId::new(), role);
        assert!(Auth::from_principal(principal(Role::Member))
            .require(Action::Write, "conversation")
            .is_ok());
        assert_eq!(
            Auth::from_principal(principal(Role::Viewer))
                .require(Action::Write, "conversation")
                .unwrap_err()
                .status(),
            axum::http::StatusCode::FORBIDDEN
        );
    }

    #[test]
    fn decision_input_requires_state_and_questions() {
        assert!(serde_json::from_value::<DecisionInput>(json!({"questions":{}})).is_err());
        assert!(serde_json::from_value::<DecisionInput>(json!({"state":{}})).is_err());
        assert!(serde_json::from_value::<DecisionInput>(
            json!({"state":{},"questions":{},"messages":[]})
        )
        .is_err());
    }

    #[tokio::test]
    async fn decisions_rest_and_tool_use_acting_user_settings_and_authenticate_before_spend() {
        use catalerum_bus::Bus;
        use catalerum_core::tool::{ToolContext, ToolRegistry};
        use catalerum_iam::{IamService, PgIamStore};
        use catalerum_llm::OpenRouterClient;
        use catalerum_store::Store;
        use tower::ServiceExt;
        use wiremock::{
            matchers::{body_partial_json, method, path, query_param},
            Mock, MockServer, ResponseTemplate,
        };
        let Ok(url) = std::env::var("CATALERUM_TEST_DATABASE_URL") else {
            return;
        };
        let store: Store = crate::test_db::isolated_store(&url).await;
        let workspace = store
            .workspaces()
            .create("decisions", &format!("decisions-{}", uuid::Uuid::new_v4()))
            .await
            .unwrap();
        let user = store
            .users()
            .create(
                &format!("decision-{}@test.invalid", uuid::Uuid::new_v4()),
                "Decision user",
                None,
            )
            .await
            .unwrap();
        store
            .memberships()
            .upsert(workspace.id, user.id, Role::Member)
            .await
            .unwrap();
        store
            .llm_settings()
            .set(
                workspace.id,
                user.id,
                Some("user-chat"),
                None,
                None,
                None,
                1.0,
                None,
                Some("user-decision"),
            )
            .await
            .unwrap();
        let iam = IamService::new(PgIamStore::new(store.pool().clone()));
        let member = iam
            .issue_session_with_role(workspace.id, user.id, Role::Member)
            .await
            .unwrap();
        let viewer_user = store
            .users()
            .create(
                &format!("viewer-{}@test.invalid", uuid::Uuid::new_v4()),
                "Viewer",
                None,
            )
            .await
            .unwrap();
        store
            .memberships()
            .upsert(workspace.id, viewer_user.id, Role::Viewer)
            .await
            .unwrap();
        let viewer = iam
            .issue_session(workspace.id, viewer_user.id)
            .await
            .unwrap();
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/v1/models")).and(query_param("type", "decisions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":[{"id":"user-decision","name":"User decision"},{"id":"explicit-decision","name":"Explicit decision"}]})))
            .expect(3).mount(&server).await;
        for (model, count) in [("user-decision", 2), ("explicit-decision", 1)] {
            Mock::given(method("POST")).and(path("/v1/decisions")).and(body_partial_json(json!({"model":model})))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"model":model,"answers":{"outcome":{"type":"noul","noul":0.75}},"usage":{"input_tokens":5,"output_tokens":1}})))
                .expect(count).mount(&server).await;
        }
        let mut config = crate::Config::default();
        config.llm.decision_model = Some("server-decision".into());
        let state = AppState::new(
            store.clone(),
            iam,
            OpenRouterClient::new(server.uri(), "test"),
            Bus::in_process(),
            config,
            None,
            None,
            None,
            None,
            std::collections::HashMap::new(),
            None,
            Vec::new(),
            None,
        );
        let input = json!({"state":{"subject":"Urgent"},"questions":{"outcome":{"type":"noul","instructions":"Urgent?"}}});
        let app = router().with_state(state.clone());
        for (token, status) in [
            (None, axum::http::StatusCode::UNAUTHORIZED),
            (
                Some(viewer.token.as_str()),
                axum::http::StatusCode::FORBIDDEN,
            ),
        ] {
            let mut request = axum::http::Request::builder()
                .method("POST")
                .uri("/decisions")
                .header("content-type", "application/json");
            if let Some(token) = token {
                request = request.header("authorization", format!("Bearer {token}"));
            }
            let response = app
                .clone()
                .oneshot(
                    request
                        .body(axum::body::Body::from(input.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), status);
        }
        assert!(server.received_requests().await.unwrap().is_empty());
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/decisions")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {}", member.token))
                    .body(axum::body::Body::from(input.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let output: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(output["model"], json!("user-decision"));
        assert_eq!(output["data"]["outcome"], json!(0.75));
        assert!(state
            .registry()
            .specs(None)
            .iter()
            .any(|tool| tool.name == "decide"));
        let ctx = ToolContext {
            workspace_id: Some(workspace.id),
            user_id: Some(user.id),
            capabilities: Some(catalerum_iam::base_capabilities(Role::Member)),
            ..Default::default()
        };
        let mut explicit = input.clone();
        explicit["model"] = json!("explicit-decision");
        let output = state
            .registry()
            .dispatch("decide", explicit, &ctx)
            .await
            .unwrap();
        assert_eq!(output["model"], json!("explicit-decision"));
        let denied = ToolContext {
            capabilities: Some(catalerum_iam::base_capabilities(Role::Viewer)),
            ..ctx
        };
        assert!(state
            .registry()
            .dispatch("decide", input.clone(), &denied)
            .await
            .is_err());
        let grant = store
            .grants()
            .upsert(
                workspace.id,
                "cost-limited",
                &catalerum_iam::base_capabilities(Role::Member),
                &catalerum_core::capability::Constraints {
                    cost_limit: Some(10.0),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let limited = ToolContext {
            workspace_id: Some(workspace.id),
            user_id: Some(user.id),
            grant_id: Some(grant.id),
            capabilities: Some(grant.capabilities.clone()),
            ..Default::default()
        };
        assert!(state
            .registry()
            .dispatch("decide", input.clone(), &limited)
            .await
            .unwrap_err()
            .to_string()
            .contains("cost_limit"));
        let limited_auth =
            Auth::with_grant(Principal::new(user.id, workspace.id, Role::Member), grant);
        let limited_input: DecisionInput = serde_json::from_value(input.clone()).unwrap();
        assert_eq!(
            decide(State(state.clone()), limited_auth, Json(limited_input))
                .await
                .unwrap_err()
                .status(),
            axum::http::StatusCode::FORBIDDEN
        );
        let dry_grant = store
            .grants()
            .upsert(
                workspace.id,
                "dry",
                &catalerum_iam::base_capabilities(Role::Member),
                &catalerum_core::capability::Constraints {
                    dry_run: true,
                    cost_limit: Some(0.0),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let dry_ctx = ToolContext {
            workspace_id: Some(workspace.id),
            user_id: Some(user.id),
            grant_id: Some(dry_grant.id),
            capabilities: Some(dry_grant.capabilities.clone()),
            ..Default::default()
        };
        let output = state
            .registry()
            .dispatch("decide", input.clone(), &dry_ctx)
            .await
            .unwrap();
        assert_eq!(output, json!({"dry_run":true,"model":"user-decision"}));
        let dry_auth = Auth::with_grant(
            Principal::new(user.id, workspace.id, Role::Member),
            dry_grant,
        );
        let dry_input: DecisionInput = serde_json::from_value(input.clone()).unwrap();
        let output = decide(State(state.clone()), dry_auth, Json(dry_input))
            .await
            .unwrap()
            .0;
        assert_eq!(output, json!({"dry_run":true,"model":"user-decision"}));
        let mcp = crate::routes::mcp::router().with_state(state.clone());
        for (id, is_error) in [
            (limited.grant_id.unwrap(), true),
            (dry_ctx.grant_id.unwrap(), false),
        ] {
            let session = state
                .iam()
                .issue_session_with_ttl_days(workspace.id, user.id, Role::Member, Some(id), 1)
                .await
                .unwrap();
            let request = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"decide","arguments":input}});
            let response = mcp
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .method("POST")
                        .uri("/mcp")
                        .header("content-type", "application/json")
                        .header("authorization", format!("Bearer {}", session.token))
                        .body(axum::body::Body::from(request.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), axum::http::StatusCode::OK);
            let output: Value = serde_json::from_slice(
                &axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(output["result"]["isError"], json!(is_error), "{output}");
            let text = output["result"]["content"][0]["text"].as_str().unwrap();
            if is_error {
                assert!(text.contains("cost_limit"));
            } else {
                assert_eq!(
                    serde_json::from_str::<Value>(text).unwrap()["dry_run"],
                    json!(true)
                );
            }
        }
        let delegate_args = input.clone();
        Mock::given(method("POST")).and(path("/v1/responses"))
            .respond_with(move |request: &wiremock::Request| {
                let request: Value = serde_json::from_slice(&request.body).unwrap();
                let returned = request["input"].as_array().unwrap().iter().find(|item| item["type"] == "function_call_output");
                let output = if let Some(returned) = returned {
                    assert!(returned["output"].as_str().unwrap().contains("cost_limit"));
                    json!([{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Decision refused by grant"}]}])
                } else {
                    json!([{"type":"function_call","call_id":"decide-1","name":"decide","arguments":delegate_args.to_string()}])
                };
                let event = json!({"type":"response.completed","response":{"id":"r1","object":"response","status":"completed","model":"chat-model","output":output}});
                let text_delta = if returned.is_some() {
                    format!("event: response.output_text.delta\ndata: {}\n\n", json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"Decision refused by grant"}))
                } else {
                    format!("event: response.output_item.added\ndata: {}\n\n", json!({"type":"response.output_item.added","output_index":0,"item":output[0]}))
                };
                ResponseTemplate::new(200).insert_header("content-type","text/event-stream").set_body_string(format!("{text_delta}event: response.completed\ndata: {event}\n\n"))
            }).expect(2).mount(&server).await;
        let delegated = crate::profile_agent::registry_with_delegate(
            state.registry(),
            &store,
            state.llm(),
            workspace.id,
            "chat-model",
            "chat-model",
            limited.capabilities.clone().unwrap(),
            Vec::new(),
            None,
            state.subagent_runs(),
        );
        let output = delegated
            .dispatch(
                "delegate",
                json!({"task":"Classify this","tools":["decide"]}),
                &limited,
            )
            .await
            .unwrap();
        assert_eq!(output["content"], json!("Decision refused by grant"));
        assert_eq!(output["tool_calls"], json!(1));
        let runner = crate::ToolActionRunner::workspace_owner_authority(ToolRegistry::new(), store)
            .as_user(user.id)
            .with_llm(state.llm().clone(), "server-chat")
            .with_decision_model(Some("server-decision".into()));
        let action = catalerum_automation::Action {
            kind: catalerum_automation::ActionKind::LlmDecision,
            params: input.as_object().unwrap().clone(),
        };
        let result =
            catalerum_automation::ActionRunner::run(&runner, workspace.id, &action, None, None)
                .await;
        assert_eq!(
            result.status,
            catalerum_core::model::StepStatus::Succeeded,
            "{:?}",
            result.error
        );
        assert_eq!(result.output.unwrap()["model"], json!("user-decision"));
    }
}
