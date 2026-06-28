//! Agent-profile management (SOUL §19/§25) — define and manage the durable,
//! named scoped-agent configurations that bind a model, a tool/skill set, the
//! subagents a profile may delegate to, the channels it listens on, and the §19
//! `grant` that is its authority. A profile is the persisted form of the §19
//! agent; it exists so *separate, securely-scoped data access* is a first-class
//! object (a "calendar bot" profile literally cannot read storage).
//!
//! - `GET    /agent-profiles`        list this workspace's profiles (by name)
//! - `POST   /agent-profiles`        create a profile (`201`; `409` if the name exists)
//! - `GET    /agent-profiles/{name}` fetch one profile by name (`404` if absent)
//! - `PUT    /agent-profiles/{name}` create-or-replace the named profile
//! - `DELETE /agent-profiles/{name}` delete a profile (`204`; `404` if absent)
//!
//! **Admin-only (SOUL §19):** like grants, managing agent profiles is gated on
//! `agent_profile:read`/`write`, which no base role implies — only an Owner/Admin
//! `*` covers it (deny-by-default). Every route is workspace-scoped (§18): the
//! client never names a workspace, so cross-workspace reach is impossible.
//!
//! **Attenuation (SOUL §19):** a profile may reference only a grant in its own
//! workspace whose capabilities are ⊆ the creator's own authority — a profile can
//! never confer more than its creator holds. (Subagent ⊆ parent is enforced again
//! at delegation time; see [`crate::profile_agent`].)

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;

use catalerum_core::capability::{attenuate, Action};
use catalerum_core::model::{AgentProfile, ToolGuard};
use catalerum_core::GrantId;
use catalerum_iam::base_capabilities;
use catalerum_iam::Principal;
use catalerum_store::{NewAgentProfile, StoreError};

use catalerum_llm::catalog::ModelKind;

use crate::auth::Auth;
use crate::error::{ApiError, ApiResult};
use crate::model_validation::validate_model;
use crate::state::AppState;

/// Mount the agent-profile routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/agent-profiles", get(list).post(create))
        .route(
            "/agent-profiles/{name}",
            get(get_one).put(upsert).delete(delete_profile),
        )
}

/// Body for `POST /agent-profiles`. Only `name` is required; the rest default to
/// empty / unset.
#[derive(Debug, Deserialize)]
pub struct CreateAgentProfile {
    /// Unique (per workspace) profile name.
    pub name: String,
    /// Model id to run against; absent uses the workspace default.
    #[serde(default)]
    pub model: Option<String>,
    /// System prompt; absent uses the default agent system prompt.
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// Tool names the profile may dispatch (subset of the registry); empty = all.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Skill names whose runbooks seed the system prompt.
    #[serde(default)]
    pub skills: Vec<String>,
    /// Agent-profile names this profile may delegate to (subagents).
    #[serde(default)]
    pub subagents: Vec<String>,
    /// Channel names this profile listens on.
    #[serde(default)]
    pub channels: Vec<String>,
    /// The §19 grant id (UUID string) that is this profile's authority; absent
    /// runs under bounded base-Member capabilities.
    #[serde(default)]
    pub grant_id: Option<String>,
    /// Optional tool guard (SOUL §19): a Boa JS and/or LLM classifier gating every
    /// tool call. Absent leaves the profile gated only by its capabilities.
    #[serde(default)]
    pub guard: Option<ToolGuard>,
}

/// Body for `PUT /agent-profiles/{name}` — a full replacement; the name comes
/// from the path (create-or-replace semantics).
#[derive(Debug, Deserialize)]
pub struct UpdateAgentProfile {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub subagents: Vec<String>,
    #[serde(default)]
    pub channels: Vec<String>,
    #[serde(default)]
    pub grant_id: Option<String>,
    /// Optional tool guard (SOUL §19); see [`CreateAgentProfile::guard`].
    #[serde(default)]
    pub guard: Option<ToolGuard>,
}

/// Normalize + validate a tool guard: blank script/instruction fields are treated
/// as unset, a supplied LLM model is checked against the gateway catalog, and a
/// guard left with neither a script nor a usable LLM classifier collapses to
/// `None` (inert → stored unguarded). A malformed *script* is not rejected here —
/// it fails closed at dispatch (`on_error`, default deny), so a broken classifier
/// can never silently open the gate.
async fn clean_guard(state: &AppState, guard: Option<ToolGuard>) -> ApiResult<Option<ToolGuard>> {
    let Some(mut g) = guard else {
        return Ok(None);
    };
    if g.script.as_ref().is_some_and(|s| s.trim().is_empty()) {
        g.script = None;
    }
    if let Some(llm) = g.llm.as_mut() {
        llm.instruction = llm.instruction.trim().to_string();
        llm.model = clean_opt(llm.model.take());
        if let Some(m) = &llm.model {
            validate_model(state, "tool guard", m, ModelKind::Chat).await?;
        }
    }
    // An LLM classifier with no instruction can't judge → drop it.
    if g.llm.as_ref().is_some_and(|l| l.instruction.is_empty()) {
        g.llm = None;
    }
    // Normalize the object-label policy (trim/dedup each list); an empty policy drops.
    if let Some(policy) = g.object_labels.as_mut() {
        policy.require_any = clean_list(std::mem::take(&mut policy.require_any));
        policy.deny = clean_list(std::mem::take(&mut policy.deny));
    }
    if g.object_labels.as_ref().is_some_and(|p| p.is_empty()) {
        g.object_labels = None;
    }
    // A guard with no classifier and no label policy is inert.
    if g.script.is_none() && g.llm.is_none() && g.object_labels.is_none() {
        return Ok(None);
    }
    Ok(Some(g))
}

/// Normalize a name list: trim each, drop empties, de-duplicate preserving order.
fn clean_list(items: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(items.len());
    for item in items {
        let t = item.trim();
        if !t.is_empty() && !out.iter().any(|x| x == t) {
            out.push(t.to_string());
        }
    }
    out
}

/// Validate + normalize a profile name (the per-workspace unique key).
fn clean_name(raw: &str) -> ApiResult<String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("profile name must not be empty"));
    }
    Ok(name.to_string())
}

/// Normalize an optional string field: trim, and treat blank as unset.
fn clean_opt(raw: Option<String>) -> Option<String> {
    raw.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Resolve + authorize an optional `grant_id` for the acting principal: the grant
/// must exist in this workspace, and (SOUL §19 attenuation) its capabilities must
/// be ⊆ the creator's own authority — a profile can never confer more than its
/// creator holds. A no-op for an Owner/Admin (`*` covers everything, and only they
/// reach this gate today), but enforced so opening profile-creation to a narrower
/// role can never become an escalation.
async fn resolve_grant(
    state: &AppState,
    p: &Principal,
    grant_id: Option<String>,
) -> ApiResult<Option<GrantId>> {
    let Some(raw) = clean_opt(grant_id) else {
        return Ok(None);
    };
    let id: GrantId = raw
        .parse()
        .map_err(|_| ApiError::bad_request("invalid grant id"))?;
    let grant = state
        .store()
        .grants()
        .get(p.workspace_id, id)
        .await
        .map_err(|e| match e {
            StoreError::NotFound => ApiError::bad_request("grant not found in this workspace"),
            other => {
                tracing::error!(error = %other, "resolving profile grant");
                ApiError::internal("resolving profile grant failed")
            }
        })?;
    let base = base_capabilities(p.role);
    for cap in &grant.capabilities {
        if attenuate(&base, cap).is_err() {
            return Err(ApiError::Forbidden(
                "profile grant exceeds your own authority".to_string(),
            ));
        }
    }
    Ok(Some(id))
}

async fn list(State(state): State<AppState>, auth: Auth) -> ApiResult<Json<Vec<AgentProfile>>> {
    let p = auth.principal();
    auth.require(Action::Read, "agent_profile")?;
    let profiles = state
        .store()
        .agent_profiles()
        .list_by_workspace(p.workspace_id)
        .await?;
    Ok(Json(profiles))
}

async fn create(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<CreateAgentProfile>,
) -> ApiResult<(StatusCode, Json<AgentProfile>)> {
    let p = auth.principal();
    auth.require(Action::Write, "agent_profile")?;
    let grant_id = resolve_grant(&state, &p, body.grant_id).await?;
    let model = clean_opt(body.model);
    if let Some(m) = &model {
        // Reject a typo against the gateway's chat-model catalog (an agent runs a
        // chat model).
        validate_model(&state, "agent profile", m, ModelKind::Chat).await?;
    }
    let spec = NewAgentProfile {
        name: clean_name(&body.name)?,
        model,
        system_prompt: clean_opt(body.system_prompt),
        tools: clean_list(body.tools),
        skills: clean_list(body.skills),
        subagents: clean_list(body.subagents),
        channels: clean_list(body.channels),
        grant_id,
        guard: clean_guard(&state, body.guard).await?,
    };
    let profile = state
        .store()
        .agent_profiles()
        .create(p.workspace_id, &spec)
        .await?;
    Ok((StatusCode::CREATED, Json(profile)))
}

async fn get_one(
    State(state): State<AppState>,
    auth: Auth,
    Path(name): Path<String>,
) -> ApiResult<Json<AgentProfile>> {
    let p = auth.principal();
    auth.require(Action::Read, "agent_profile")?;
    let profile = state
        .store()
        .agent_profiles()
        .get_by_name(p.workspace_id, name.trim())
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(profile))
}

async fn upsert(
    State(state): State<AppState>,
    auth: Auth,
    Path(name): Path<String>,
    Json(body): Json<UpdateAgentProfile>,
) -> ApiResult<Json<AgentProfile>> {
    let p = auth.principal();
    auth.require(Action::Write, "agent_profile")?;
    let grant_id = resolve_grant(&state, &p, body.grant_id).await?;
    let model = clean_opt(body.model);
    if let Some(m) = &model {
        validate_model(&state, "agent profile", m, ModelKind::Chat).await?;
    }
    let spec = NewAgentProfile {
        name: clean_name(&name)?,
        model,
        system_prompt: clean_opt(body.system_prompt),
        tools: clean_list(body.tools),
        skills: clean_list(body.skills),
        subagents: clean_list(body.subagents),
        channels: clean_list(body.channels),
        grant_id,
        guard: clean_guard(&state, body.guard).await?,
    };
    let profile = state
        .store()
        .agent_profiles()
        .upsert_by_name(p.workspace_id, &spec)
        .await?;
    Ok(Json(profile))
}

async fn delete_profile(
    State(state): State<AppState>,
    auth: Auth,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    let p = auth.principal();
    auth.require(Action::Write, "agent_profile")?;
    let profile = state
        .store()
        .agent_profiles()
        .get_by_name(p.workspace_id, name.trim())
        .await?
        .ok_or(ApiError::NotFound)?;
    state
        .store()
        .agent_profiles()
        .delete(p.workspace_id, profile.id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_profile_requires_only_name() {
        let body: CreateAgentProfile = serde_json::from_str(r#"{"name":"calbot"}"#).unwrap();
        assert_eq!(body.name, "calbot");
        assert!(body.model.is_none());
        assert!(body.tools.is_empty());
        assert!(body.channels.is_empty());
        assert!(body.grant_id.is_none());
    }

    #[test]
    fn clean_list_trims_dedups_drops_empty() {
        let cleaned = clean_list(vec![
            "  get_events ".into(),
            "get_events".into(),
            String::new(),
            "  ".into(),
            "notify".into(),
        ]);
        assert_eq!(
            cleaned,
            vec!["get_events".to_string(), "notify".to_string()]
        );
    }

    #[test]
    fn clean_name_rejects_blank_and_trims() {
        assert!(clean_name("  ").is_err());
        assert_eq!(clean_name("  calbot ").unwrap(), "calbot");
    }

    #[test]
    fn clean_opt_blanks_to_none() {
        assert_eq!(clean_opt(Some("  ".into())), None);
        assert_eq!(clean_opt(None), None);
        assert_eq!(clean_opt(Some("  gpt ".into())), Some("gpt".to_string()));
    }
}
