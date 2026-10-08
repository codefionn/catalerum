//! Native decision transport over the gateway's `/v1/decisions` API.

use catalerum_core::decisions::{validate_answers, validate_questions};
use catalerum_core::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::client::{map_sdk_error, OpenRouterClient};

/// Structured state and named questions for a decision model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DecisionRequest {
    pub model: String,
    pub state: Value,
    pub questions: Map<String, Value>,
}

/// Provider accounting, preserving a zero cost and unknown usage fields.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DecisionUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
    #[serde(default, flatten)]
    pub extra: Map<String, Value>,
}

/// Raw answers retain probabilities, confidence, and score legends.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DecisionResponse {
    pub model: String,
    pub answers: Map<String, Value>,
    pub usage: DecisionUsage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, flatten)]
    pub extra: Map<String, Value>,
}

impl OpenRouterClient {
    /// Call a native decision model and validate every requested answer.
    pub async fn decide(&self, request: DecisionRequest) -> Result<DecisionResponse> {
        if request.model.trim().is_empty() {
            return Err(Error::invalid("decision model must be non-empty"));
        }
        if !matches!(
            request.state,
            Value::String(_) | Value::Object(_) | Value::Array(_)
        ) {
            return Err(Error::invalid(
                "decision state must be a string, object, or array",
            ));
        }
        validate_questions(&request.questions).map_err(Error::invalid)?;
        let questions = request.questions.clone();
        let response = self
            .sdk()?
            .decisions(llmleaf_client::DecisionsRequest::new(
                request.model,
                request.state,
                request.questions,
            ))
            .await
            .map_err(map_sdk_error)?;
        validate_answers(&questions, &response.answers).map_err(Error::provider)?;
        if response.model.trim().is_empty() {
            return Err(Error::provider("decision response model must be non-empty"));
        }
        Ok(DecisionResponse {
            model: response.model,
            answers: response.answers,
            usage: DecisionUsage {
                input_tokens: response.usage.input_tokens,
                output_tokens: response.usage.output_tokens,
                cost: response.usage.cost,
                extra: response.usage.extra,
            },
            id: response.id,
            provider: response.provider,
            extra: response.extra,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ModelKind;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn mock(status: u16, body: Value) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0; 2048];
            loop {
                let count = stream.read(&mut chunk).await.unwrap();
                assert!(count > 0, "incomplete HTTP request");
                request.extend_from_slice(&chunk[..count]);
                if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]);
                    let length = headers
                        .lines()
                        .filter_map(|line| line.split_once(':'))
                        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                        .map(|(_, value)| value.trim().parse::<usize>().unwrap())
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let body = body.to_string();
            let response = format!("HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            stream.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8(request).unwrap()
        });
        (format!("http://{address}"), task)
    }

    fn request() -> DecisionRequest {
        DecisionRequest {
            model: "jev".into(),
            state: json!({"ticket":{"text":"export is broken"},"history":[1,2]}),
            questions: json!({
                "urgent":{"type":"noul","instructions":{"question":"Urgent?"}},
                "team":{"type":"choice","instructions":"Who?","criteria":{"engineering":null,"support":"Usage questions"}},
                "severity":{"type":"score","instructions":["Rate severity"],"criteria":["None","Some","All"]}
            }).as_object().unwrap().clone(),
        }
    }

    fn response() -> Value {
        json!({
            "model":"jev-resolved", "id":"dec_1", "provider":"openrouter", "trace_id":"trace_1",
            "answers":{
                "urgent":{"type":"noul","noul":0.95},
                "team":{"type":"choice","choice":"engineering","confidence":0.8,"probabilities":{"engineering":0.8,"support":0.2}},
                "severity":{"type":"score","score":1.8,"legend":{"0":"None","2":"All"}}
            },
            "usage":{"input_tokens":12,"output_tokens":4,"cost":0.0,"cached_tokens":8}
        })
    }

    #[tokio::test]
    async fn uses_decisions_endpoint_and_preserves_structured_values_and_metadata() {
        let expected = response();
        let (base, capture) = mock(200, expected.clone()).await;
        let client = OpenRouterClient::new(format!("{base}/"), "sk-test");
        let request = request();
        let result = client.decide(request.clone()).await.unwrap();
        assert_eq!(serde_json::to_value(&result).unwrap(), expected);
        assert_eq!(result.usage.cost, Some(0.0));
        let captured = capture.await.unwrap();
        assert!(captured.starts_with("POST /v1/decisions HTTP/1.1"));
        assert!(captured
            .to_lowercase()
            .contains("authorization: bearer sk-test"));
        let body: Value = serde_json::from_str(captured.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body, serde_json::to_value(request).unwrap());
    }

    #[tokio::test]
    async fn malformed_or_missing_answers_fail_the_call() {
        for answers in [
            json!({}),
            json!({"urgent":{"type":"noul","noul":1.2}}),
            json!({"urgent":true}),
        ] {
            let mut body = response();
            body["answers"] = answers;
            let (base, capture) = mock(200, body).await;
            assert!(matches!(
                OpenRouterClient::new(base, "k").decide(request()).await,
                Err(Error::Provider(_))
            ));
            capture.await.unwrap();
        }
    }

    #[tokio::test]
    async fn provider_errors_propagate_and_invalid_input_is_rejected_before_transport() {
        let (base, capture) = mock(403, json!({"error":{"message":"model not allowed"}})).await;
        let error = OpenRouterClient::new(base, "k")
            .decide(request())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("model not allowed"));
        capture.await.unwrap();
        let client = OpenRouterClient::new("http://127.0.0.1:1", "k");
        let mut invalid = request();
        invalid.questions.clear();
        assert!(matches!(
            client.decide(invalid).await,
            Err(Error::Invalid(_))
        ));
        let mut invalid = request();
        invalid.state = json!(false);
        assert!(matches!(
            client.decide(invalid).await,
            Err(Error::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn model_catalog_has_native_decisions_filter() {
        let (base, capture) = mock(200, json!({"data":[]})).await;
        assert!(OpenRouterClient::new(base, "k")
            .list_models(ModelKind::Decisions, None)
            .await
            .unwrap()
            .is_empty());
        assert!(capture
            .await
            .unwrap()
            .starts_with("GET /v1/models?type=decisions HTTP/1.1"));
        assert_eq!(
            serde_json::to_value(ModelKind::Decisions).unwrap(),
            json!("decisions")
        );
    }
}
