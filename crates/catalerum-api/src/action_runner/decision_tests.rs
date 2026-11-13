use super::*;
use catalerum_core::{capability::Constraints, model::StepStatus, GrantId};
use wiremock::{
    matchers::{body_json, method, path, query_param},
    Mock, MockServer, ResponseTemplate,
};

fn decision_action(params: Value) -> Action {
    Action {
        kind: ActionKind::LlmDecision,
        params: params.as_object().unwrap().clone(),
    }
}

fn decision_questions() -> Value {
    json!({"outcome":{"type":"noul","instructions":"Is this urgent?"}})
}

async fn decision_catalog(server: &MockServer, model: &str) {
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(query_param("type", "decisions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data":[{"id":model,"name":"Decision model"}]})),
        )
        .expect(1)
        .mount(server)
        .await;
}

#[tokio::test]
async fn decision_runtime_projects_probabilities_and_preserves_trigger_envelope() {
    let server = MockServer::start().await;
    decision_catalog(&server, "decision-model").await;
    let context = json!({"trigger":{"kind":"email","subject":"Urgent"},"inputs":{"previous":{"count":3}}});
    let questions = decision_questions();
    Mock::given(method("POST")).and(path("/v1/decisions"))
        .and(body_json(json!({"model":"decision-model","state":context,"questions":questions})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model":"decision-model", "answers":{"outcome":{"type":"noul","noul":0.91,"confidence":0.8}},
            "usage":{"input_tokens":9,"output_tokens":1,"cost":0.0}
        }))).expect(1).mount(&server).await;
    let runner = ToolActionRunner::new(ToolRegistry::new())
        .with_llm(OpenRouterClient::new(server.uri(), "test"), "chat-model")
        .with_decision_model(Some("decision-model".into()))
        .with_capabilities(catalerum_iam::base_capabilities(Role::Member));
    let result = runner
        .run(
            WorkspaceId::new(),
            &decision_action(json!({"questions":questions})),
            Some(&context),
            None,
        )
        .await;
    assert_eq!(result.status, StepStatus::Succeeded, "{:?}", result.error);
    let out = result.output.unwrap();
    assert_eq!(out["data"]["outcome"], json!(0.91));
    assert_eq!(out["answers"]["outcome"]["confidence"], json!(0.8));
    assert_eq!(out["cost_usd"], json!(0.0));
    assert_eq!(out["usage"]["input_tokens"], json!(9));
}

#[tokio::test]
async fn decision_runtime_renders_nested_state_and_instructions_with_explicit_model() {
    let server = MockServer::start().await;
    decision_catalog(&server, "explicit-model").await;
    Mock::given(method("POST")).and(path("/v1/decisions"))
        .and(body_json(json!({"model":"explicit-model","state":{"count":3},"questions":{"outcome":{"type":"noul","instructions":"Process Urgent"}}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"model":"explicit-model","answers":{"outcome":{"type":"noul","noul":0.4}},"usage":{"input_tokens":1,"output_tokens":1}})))
        .expect(1).mount(&server).await;
    let runner = ToolActionRunner::new(ToolRegistry::new())
        .with_llm(OpenRouterClient::new(server.uri(), "test"), "chat-model")
        .with_decision_model(Some("server-model".into()));
    let action = decision_action(
        json!({"model":"explicit-model","state":"{{ inputs.previous }}","questions":{"outcome":{"type":"noul","instructions":"Process {{ trigger.subject }}"}}}),
    );
    let result = runner
        .run(
            WorkspaceId::new(),
            &action,
            Some(&json!({"trigger":{"subject":"Urgent"},"inputs":{"previous":{"count":3}}})),
            None,
        )
        .await;
    assert_eq!(result.status, StepStatus::Succeeded, "{:?}", result.error);
}

#[tokio::test]
async fn decision_runtime_missing_model_dry_run_cost_limit_and_denial_never_spend() {
    let server = MockServer::start().await;
    let runner = ToolActionRunner::new(ToolRegistry::new())
        .with_llm(OpenRouterClient::new(server.uri(), "test"), "chat-model");
    let action = decision_action(json!({"questions":decision_questions()}));
    let ws = WorkspaceId::new();
    let missing = runner.run(ws, &action, None, None).await;
    assert!(missing.error.unwrap().contains("no decision model"));
    let runner = runner.with_decision_model(Some("decision-model".into()));
    let denied = runner
        .clone()
        .with_capabilities(catalerum_iam::base_capabilities(Role::Viewer))
        .run(ws, &action, None, None)
        .await;
    assert!(denied.error.unwrap().contains("conversation:write"));
    let mut grant = Grant {
        id: GrantId::new(),
        workspace_id: ws,
        name: "limited".into(),
        capabilities: catalerum_iam::base_capabilities(Role::Member),
        constraints: Constraints {
            dry_run: true,
            cost_limit: Some(1.0),
            ..Default::default()
        },
    };
    let dry = runner.run(ws, &action, None, Some(&grant)).await;
    assert_eq!(dry.status, StepStatus::Succeeded);
    assert_eq!(
        dry.output.unwrap(),
        json!({"dry_run":true,"model":"decision-model"})
    );
    grant.constraints.dry_run = false;
    let limited = runner.run(ws, &action, None, Some(&grant)).await;
    assert!(limited.error.unwrap().contains("cost_limit"));
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn decision_runtime_rejects_missing_answers_and_wrong_model_kind() {
    let server = MockServer::start().await;
    decision_catalog(&server, "decision-model").await;
    Mock::given(method("POST")).and(path("/v1/decisions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"model":"decision-model","answers":{},"usage":{"input_tokens":1,"output_tokens":1}})))
        .expect(1).mount(&server).await;
    let runner = ToolActionRunner::new(ToolRegistry::new())
        .with_llm(OpenRouterClient::new(server.uri(), "test"), "chat-model");
    let result = runner
        .run(
            WorkspaceId::new(),
            &decision_action(
                json!({"model":"decision-model","state":{},"questions":decision_questions()}),
            ),
            None,
            None,
        )
        .await;
    assert_eq!(result.status, StepStatus::Failed);
    assert!(result.error.unwrap().contains("outcome"));

    let server = MockServer::start().await;
    decision_catalog(&server, "decision-model").await;
    let runner = ToolActionRunner::new(ToolRegistry::new())
        .with_llm(OpenRouterClient::new(server.uri(), "test"), "chat-model");
    let result = runner
        .run(
            WorkspaceId::new(),
            &decision_action(json!({"model":"chat-model","questions":decision_questions()})),
            None,
            None,
        )
        .await;
    assert_eq!(result.status, StepStatus::Failed);
    assert!(result.error.unwrap().contains("decisions catalog"));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn decision_runtime_allows_explicit_pinned_id_when_catalog_is_empty() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(query_param("type", "decisions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":[]})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST")).and(path("/v1/decisions"))
        .and(body_json(json!({"model":"jev/version","state":{},"questions":decision_questions()})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"model":"jev/version","answers":{"outcome":{"type":"noul","noul":0.9}},"usage":{"input_tokens":1,"output_tokens":1}})))
        .expect(1).mount(&server).await;
    let runner = ToolActionRunner::new(ToolRegistry::new())
        .with_llm(OpenRouterClient::new(server.uri(), "test"), "chat-model");
    let result = runner
        .run(
            WorkspaceId::new(),
            &decision_action(json!({"model":"jev/version","questions":decision_questions()})),
            None,
            None,
        )
        .await;
    assert_eq!(result.status, StepStatus::Succeeded, "{:?}", result.error);
}
