//! Skills REST (SOUL §23) — author and manage a workspace's skills.
//!
//! Workspace-scoped to the authenticated principal (SOUL §18): the client never
//! names a workspace, so cross-workspace reach is impossible by construction.
//!
//! **Capability-gated (SOUL §19)** via the shared [`Auth::require`] gate, like the
//! other data REST surfaces (notes, calendar, conversations). Reads require
//! `skill:read` (every role holds it); writes — create / replace / delete —
//! require `skill:write`, which a Viewer does **not** hold, so a Viewer is `403
//! Forbidden` (deny-by-default). The matching LLM tools (`use_skill`/`list_skills`/
//! `create_skill`/`edit_skill`, [`crate::tools`]) enforce the same model per call,
//! including the per-skill `skill:use@<name>` selector.
//!
//! Routes:
//! - `GET    /skills`          list this workspace's skills (by name)
//! - `POST   /skills`          create a skill (`201`; `409` if the name exists)
//! - `GET    /skills/{name}`   fetch one skill by name (`404` if absent)
//! - `PUT    /skills/{name}`   create-or-replace the named skill
//! - `DELETE /skills/{name}`   delete a skill (`204`; `404` if absent)

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;

use catalerum_core::capability::Action;
use catalerum_core::model::{Code, Skill};
use catalerum_store::NewSkill;

use crate::auth::Auth;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Mount the skills routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/skills", get(list).post(create))
        .route(
            "/skills/{name}",
            get(get_one).put(upsert).delete(delete_skill),
        )
}

/// Body for `POST /skills`. Only `name` is required; the rest default to empty.
#[derive(Debug, Deserialize)]
pub struct CreateSkill {
    /// Unique (per workspace) skill name — how the skill is invoked.
    pub name: String,
    /// One-line description.
    #[serde(default)]
    pub description: String,
    /// Markdown runbook / instructions.
    #[serde(default)]
    pub instructions_md: String,
    /// Tool names the skill may use (a subset of the registry).
    #[serde(default)]
    pub tools: Vec<String>,
    /// Optional executable code (run via the Executor §20).
    #[serde(default)]
    pub code: Option<Code>,
    /// Whether the skill's name + description are advertised to the chat agent
    /// in its system prompt ("visible to agent"). Defaults to `true`.
    #[serde(default = "default_true")]
    pub advertised: bool,
}

/// Body for `PUT /skills/{name}` — a full replacement; the name comes from the
/// path (create-or-replace semantics).
#[derive(Debug, Deserialize)]
pub struct UpdateSkill {
    /// New one-line description.
    #[serde(default)]
    pub description: String,
    /// New markdown runbook / instructions.
    #[serde(default)]
    pub instructions_md: String,
    /// New tool set (replaces the existing tools).
    #[serde(default)]
    pub tools: Vec<String>,
    /// New optional code (replaces / clears the existing code).
    #[serde(default)]
    pub code: Option<Code>,
    /// New "visible to agent" flag (full-replacement semantics like the rest;
    /// an omitted field lands the default `true`).
    #[serde(default = "default_true")]
    pub advertised: bool,
}

/// serde default for a `bool` body field that should default to `true`.
fn default_true() -> bool {
    true
}

/// Normalize a tool-name list: trim each, drop empties, de-duplicate while
/// preserving order — keeps a skill's stored tool set clean regardless of input.
/// `pub(crate)`: the `create_skill`/`edit_skill` LLM tools ([`crate::tools`])
/// normalize through this same helper, so tool-authored and user-authored skills
/// get identical tool-list handling (the `clean_tags` pattern).
pub(crate) fn clean_tools(tools: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(tools.len());
    for tool in tools {
        let t = tool.trim();
        if !t.is_empty() && !out.iter().any(|x| x == t) {
            out.push(t.to_string());
        }
    }
    out
}

/// Validate + normalize a skill name (the per-workspace unique key): non-empty
/// after trimming.
fn clean_name(raw: &str) -> ApiResult<String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("skill name must not be empty"));
    }
    Ok(name.to_string())
}

async fn list(State(state): State<AppState>, auth: Auth) -> ApiResult<Json<Vec<Skill>>> {
    let p = auth.principal();
    auth.require(Action::Read, "skill")?;
    let skills = state
        .store()
        .skills()
        .list_by_workspace(p.workspace_id)
        .await?;
    Ok(Json(skills))
}

async fn create(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<CreateSkill>,
) -> ApiResult<(StatusCode, Json<Skill>)> {
    let p = auth.principal();
    auth.require(Action::Write, "skill")?;
    let spec = NewSkill {
        name: clean_name(&body.name)?,
        description: body.description.trim().to_string(),
        instructions_md: body.instructions_md,
        tools: clean_tools(body.tools),
        code: body.code,
        advertised: body.advertised,
    };
    let skill = state.store().skills().create(p.workspace_id, &spec).await?;
    Ok((StatusCode::CREATED, Json(skill)))
}

async fn get_one(
    State(state): State<AppState>,
    auth: Auth,
    Path(name): Path<String>,
) -> ApiResult<Json<Skill>> {
    let p = auth.principal();
    auth.require(Action::Read, "skill")?;
    let skill = state
        .store()
        .skills()
        .get_by_name(p.workspace_id, name.trim())
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(skill))
}

async fn upsert(
    State(state): State<AppState>,
    auth: Auth,
    Path(name): Path<String>,
    Json(body): Json<UpdateSkill>,
) -> ApiResult<Json<Skill>> {
    let p = auth.principal();
    auth.require(Action::Write, "skill")?;
    let spec = NewSkill {
        name: clean_name(&name)?,
        description: body.description.trim().to_string(),
        instructions_md: body.instructions_md,
        tools: clean_tools(body.tools),
        code: body.code,
        advertised: body.advertised,
    };
    let skill = state
        .store()
        .skills()
        .upsert_by_name(p.workspace_id, &spec)
        .await?;
    Ok(Json(skill))
}

async fn delete_skill(
    State(state): State<AppState>,
    auth: Auth,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    let p = auth.principal();
    auth.require(Action::Write, "skill")?;
    // Resolve by name (the public key) then delete by id, both workspace-scoped.
    let skill = state
        .store()
        .skills()
        .get_by_name(p.workspace_id, name.trim())
        .await?
        .ok_or(ApiError::NotFound)?;
    state
        .store()
        .skills()
        .delete(p.workspace_id, skill.id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_skill_requires_only_name() {
        let body: CreateSkill = serde_json::from_str(r#"{"name":"summarize"}"#).unwrap();
        assert_eq!(body.name, "summarize");
        assert!(body.description.is_empty());
        assert!(body.instructions_md.is_empty());
        assert!(body.tools.is_empty());
        assert!(body.code.is_none());
        assert!(body.advertised, "visible to agent by default");
    }

    #[test]
    fn skill_bodies_parse_advertised_opt_out() {
        let body: CreateSkill =
            serde_json::from_str(r#"{"name":"quiet","advertised":false}"#).unwrap();
        assert!(!body.advertised);
        let body: UpdateSkill = serde_json::from_str(r#"{"advertised":false}"#).unwrap();
        assert!(!body.advertised);
        let body: UpdateSkill = serde_json::from_str("{}").unwrap();
        assert!(body.advertised, "an omitted flag lands the default true");
    }

    #[test]
    fn create_skill_parses_code() {
        let body: CreateSkill = serde_json::from_str(
            r#"{"name":"run","code":{"language":"python","source":"print(1)"}}"#,
        )
        .unwrap();
        let code = body.code.unwrap();
        assert_eq!(code.language, "python");
        assert_eq!(code.entrypoint, None);
    }

    #[test]
    fn clean_tools_trims_dedups_drops_empty() {
        let cleaned = clean_tools(vec![
            "  read_note ".into(),
            "read_note".into(),
            String::new(),
            "  ".into(),
            "kanban_create_task".into(),
        ]);
        assert_eq!(
            cleaned,
            vec!["read_note".to_string(), "kanban_create_task".to_string()]
        );
    }

    #[test]
    fn clean_name_rejects_blank_and_trims() {
        assert!(clean_name("  ").is_err());
        assert_eq!(clean_name("  triage-inbox ").unwrap(), "triage-inbox");
    }
}
