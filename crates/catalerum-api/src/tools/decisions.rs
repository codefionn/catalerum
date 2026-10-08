//! Native typed decisions shared by the REST, tool, and automation callers.

use super::*;
use catalerum_llm::{DecisionRequest, DecisionResponse};
use serde::Deserialize;

/// Request accepted by the `decide` tool and `POST /decisions`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionInput {
    #[serde(default)]
    pub model: Option<String>,
    pub state: Json,
    pub questions: serde_json::Map<String, Json>,
}

impl DecisionInput {
    pub(crate) fn validate(&self) -> Result<()> {
        if !matches!(
            self.state,
            Json::String(_) | Json::Object(_) | Json::Array(_)
        ) {
            return Err(Error::invalid(
                "decision state must be a string, object, or array",
            ));
        }
        catalerum_core::decisions::validate_questions(&self.questions).map_err(Error::invalid)
    }
}

/// Resolve only decision-specific model selections. Chat defaults never enter here.
pub(crate) fn decision_model(
    explicit: Option<&str>,
    user: Option<&str>,
    server: Option<&str>,
) -> Result<String> {
    explicit
        .or(user)
        .or(server)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| Error::invalid("no decision model configured; select a decisions model explicitly or in LLM settings"))
}

/// A single provider request has no hard spend ceiling. Refuse a limited grant
/// before spending instead of treating usage reported afterwards as enforcement.
pub(crate) fn check_decision_cost_limit(cost_limit: Option<f64>) -> Result<()> {
    if cost_limit.is_some() {
        return Err(Error::invalid(
            "decisions cannot enforce this grant's cost_limit before the provider request",
        ));
    }
    Ok(())
}

/// Reject models excluded by a fetched nonempty decision catalog. An unavailable
/// or empty catalog permits explicit IDs, matching settings validation.
pub(crate) async fn validate_decision_model(llm: &OpenRouterClient, model: &str) -> Result<()> {
    let catalog = llm.list_models(ModelKind::Decisions, None).await.ok();
    if crate::model_validation::model_in_catalog(catalog.as_deref(), model) {
        Ok(())
    } else {
        Err(Error::invalid(format!("decision model `{model}` is not in the decisions catalog; use search_models with kind `decisions`")))
    }
}

/// Keep provider answer objects and expose the scalar used by branch expressions.
pub(crate) fn decision_output(response: DecisionResponse) -> Result<Json> {
    let mut data = serde_json::Map::new();
    for (name, answer) in &response.answers {
        let field = match answer.get("type").and_then(Json::as_str) {
            Some("noul") => "noul",
            Some("choice") => "choice",
            Some("score") => "score",
            _ => return Err(Error::other(format!("malformed decision answer `{name}`"))),
        };
        let value = answer
            .get(field)
            .ok_or_else(|| Error::other(format!("missing {field} in decision answer `{name}`")))?;
        let valid = if field == "choice" {
            value.is_string()
        } else {
            value.as_f64().is_some()
        };
        if !valid {
            return Err(Error::other(format!(
                "malformed {field} in decision answer `{name}`"
            )));
        }
        data.insert(name.clone(), value.clone());
    }
    let mut out = serde_json::to_value(&response).map_err(Error::other)?;
    out["data"] = Json::Object(data);
    if let Some(cost) = response.usage.cost {
        out["cost_usd"] = json!(cost);
    }
    Ok(out)
}

pub(crate) struct DecideTool {
    llm: OpenRouterClient,
    store: Store,
    default_model: Option<String>,
}

#[async_trait]
impl Tool for DecideTool {
    fn name(&self) -> &str {
        "decide"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "conversation")
    }
    fn description(&self) -> &str {
        "Answer typed decision questions about JSON state. Each question has type `noul` (probability of truth), `choice` (one allowed option), or `score` (numeric score on ordered criteria). Uses the native decisions endpoint. Returns detailed `answers`, scalar `data`, model, and usage. Discover compatible models with search_models kind `decisions`."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "model": {"type": "string", "description": "Decision model id. Omit to use your decision setting, then the server decision default."},
                "state": {"type": ["string", "object", "array"], "description": "JSON data to evaluate."},
                "questions": {"type": "object", "minProperties": 1, "additionalProperties": {
                    "oneOf": [
                        {"type":"object", "properties":{
                            "type":{"const":"noul"},
                            "instructions":{"type":["string","object","array"]},
                            "criteria":{"type":"object","additionalProperties":{"type":["string","object","array"]}}
                        }, "required":["type","instructions"]},
                        {"type":"object", "properties":{
                            "type":{"const":"choice"},
                            "instructions":{"type":["string","object","array"]},
                            "criteria":{"type":"object","minProperties":1,"additionalProperties":{"type":["null","string","object","array"]},"description":"Allowed choices mapped to their criteria."}
                        }, "required":["type","instructions","criteria"]},
                        {"type":"object", "properties":{
                            "type":{"const":"score"},
                            "instructions":{"type":["string","object","array"]},
                            "criteria":{"type":"array","minItems":1,"items":{"type":["string","object","array"]},"description":"Ordered score criteria, with scale positions starting at zero."}
                        }, "required":["type","instructions","criteria"]}
                    ]
                }}
            },
            "required": ["state", "questions"],
            "additionalProperties": false
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let input: DecisionInput = serde_json::from_value(args).map_err(Error::invalid)?;
        input.validate()?;
        let settings = if input.model.is_none() {
            user_llm_settings(&self.store, ws, ctx).await
        } else {
            None
        };
        let model = decision_model(
            input.model.as_deref(),
            settings.as_ref().and_then(|s| s.decision_model.as_deref()),
            self.default_model.as_deref(),
        )?;
        if ctx.dry_run {
            return Ok(json!({"dry_run": true, "model": model}));
        }
        if let Some(id) = ctx.grant_id {
            let grant = self
                .store
                .grants()
                .get(ws, id)
                .await
                .map_err(Error::other)?;
            if grant.constraints.dry_run {
                return Ok(json!({"dry_run": true, "model": model}));
            }
            check_decision_cost_limit(grant.constraints.cost_limit)?;
        }
        validate_decision_model(&self.llm, &model).await?;
        decision_output(
            self.llm
                .decide(DecisionRequest {
                    model,
                    state: input.state,
                    questions: input.questions,
                })
                .await?,
        )
    }
}

pub(crate) fn register_decision_tool(
    registry: &mut ToolRegistry,
    llm: OpenRouterClient,
    store: Store,
    default_model: Option<String>,
) {
    registry.register(Arc::new(DecideTool {
        llm,
        store,
        default_model,
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_output_retains_choice_probabilities_and_score_legend() {
        let response: DecisionResponse = serde_json::from_value(json!({
            "model":"jev", "answers":{
                "team":{"type":"choice","choice":"support","probabilities":{"support":0.8,"engineering":0.2}},
                "severity":{"type":"score","score":1.8,"legend":{"0":"None","2":"All"},"probabilities":{"0":0.1,"2":0.9}}
            }, "usage":{"input_tokens":2,"output_tokens":1,"cost":0.0}
        })).unwrap();
        let output = decision_output(response).unwrap();
        assert_eq!(output["data"], json!({"team":"support","severity":1.8}));
        assert_eq!(
            output["answers"]["team"]["probabilities"]["support"],
            json!(0.8)
        );
        assert_eq!(output["answers"]["severity"]["legend"]["2"], json!("All"));
    }

    #[test]
    fn decision_model_uses_only_decision_preferences() {
        assert_eq!(
            decision_model(Some(" explicit "), Some("user"), Some("server")).unwrap(),
            "explicit"
        );
        assert_eq!(
            decision_model(None, Some("user"), Some("server")).unwrap(),
            "user"
        );
        assert_eq!(
            decision_model(None, None, Some("server")).unwrap(),
            "server"
        );
        assert!(decision_model(None, None, None).is_err());
        assert!(decision_model(Some(" "), Some("user"), Some("server")).is_err());
    }

    #[test]
    fn decision_cost_limit_refuses_unbounded_provider_spend() {
        assert!(check_decision_cost_limit(None).is_ok());
        assert!(check_decision_cost_limit(Some(0.0)).is_err());
        assert!(check_decision_cost_limit(Some(100.0)).is_err());
    }
}
