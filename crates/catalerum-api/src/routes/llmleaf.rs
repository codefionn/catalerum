//! llmleaf dynamic-topology control plane. llmleaf polls the internal endpoint;
//! workspace administrators manage the provider/route overlay through the
//! authenticated operator endpoints.

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auth::Auth;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/internal/llmleaf/topology", get(topology))
        .route("/llmleaf/{kind}", get(list))
        // OpenRouter's canonical model ids are namespaced (`author/model`), so
        // a managed route name must be allowed to span more than one path
        // segment. Providers still use ordinary one-segment names; the wildcard
        // handles both without changing the wire shape.
        .route("/llmleaf/{kind}/{*name}", put(upsert).delete(remove))
}

#[derive(Debug, Serialize)]
struct TopologyResponse {
    providers: Vec<Value>,
    routes: Vec<Value>,
}

fn kind(raw: &str) -> ApiResult<&'static str> {
    match raw {
        "providers" | "provider" => Ok("provider"),
        "routes" | "route" => Ok("route"),
        _ => Err(ApiError::bad_request("kind must be providers or routes")),
    }
}

async fn topology(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<TopologyResponse>> {
    let expected = state.config().llm.control_token.expose();
    if expected.is_empty() {
        return Err(ApiError::NotFound);
    }
    let supplied = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if supplied != Some(expected) {
        return Err(ApiError::unauthorized("invalid llmleaf control token"));
    }
    let providers = state
        .store()
        .llmleaf_topology()
        .list("provider", true)
        .await?
        .into_iter()
        .map(|entry| entry.spec.0)
        .collect();
    let routes = state
        .store()
        .llmleaf_topology()
        .list("route", true)
        .await?
        .into_iter()
        .map(|entry| entry.spec.0)
        .collect();
    Ok(Json(TopologyResponse { providers, routes }))
}

#[derive(Debug, Serialize)]
struct EntryResponse {
    kind: String,
    name: String,
    enabled: bool,
    spec: Value,
}

impl From<catalerum_store::LlmleafTopologyEntry> for EntryResponse {
    fn from(entry: catalerum_store::LlmleafTopologyEntry) -> Self {
        Self {
            kind: entry.kind,
            name: entry.name,
            enabled: entry.enabled,
            spec: entry.spec.0,
        }
    }
}

async fn list(
    State(state): State<AppState>,
    auth: Auth,
    Path(raw_kind): Path<String>,
) -> ApiResult<Json<Vec<EntryResponse>>> {
    auth.require_workspace_admin()?;
    let entries = state
        .store()
        .llmleaf_topology()
        .list(kind(&raw_kind)?, false)
        .await?
        .into_iter()
        .map(Into::into)
        .collect();
    Ok(Json(entries))
}

#[derive(Debug, Deserialize)]
struct UpsertBody {
    spec: Value,
    #[serde(default = "enabled_default")]
    enabled: bool,
}

const fn enabled_default() -> bool {
    true
}

fn validate_spec(kind: &str, name: &str, spec: &Value) -> ApiResult<()> {
    let object = spec
        .as_object()
        .ok_or_else(|| ApiError::bad_request("spec must be a JSON object"))?;
    let key = if kind == "provider" { "name" } else { "model" };
    if object.get(key).and_then(Value::as_str) != Some(name) {
        return Err(ApiError::bad_request(format!(
            "spec.{key} must match the resource name"
        )));
    }
    if kind == "provider" {
        if object.get("kind").and_then(Value::as_str).is_none() {
            return Err(ApiError::bad_request("provider spec.kind is required"));
        }
        if let Some(credential) = object.get("credential").and_then(Value::as_str) {
            if !credential.starts_with("env:") {
                return Err(ApiError::bad_request(
                    "provider credentials must use llmleaf env:VAR indirection",
                ));
            }
        }
    } else if object
        .get("targets")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty)
    {
        return Err(ApiError::bad_request(
            "route spec.targets must not be empty",
        ));
    }
    Ok(())
}

async fn upsert(
    State(state): State<AppState>,
    auth: Auth,
    Path((raw_kind, name)): Path<(String, String)>,
    Json(body): Json<UpsertBody>,
) -> ApiResult<Json<EntryResponse>> {
    auth.require_workspace_admin()?;
    let entry_kind = kind(&raw_kind)?;
    validate_spec(entry_kind, &name, &body.spec)?;
    Ok(Json(
        state
            .store()
            .llmleaf_topology()
            .upsert(entry_kind, &name, body.spec, body.enabled)
            .await?
            .into(),
    ))
}

async fn remove(
    State(state): State<AppState>,
    auth: Auth,
    Path((raw_kind, name)): Path<(String, String)>,
) -> ApiResult<axum::http::StatusCode> {
    auth.require_workspace_admin()?;
    state
        .store()
        .llmleaf_topology()
        .delete(kind(&raw_kind)?, &name)
        .await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}
