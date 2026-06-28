//! Tool catalog REST (SOUL §7/§19/§25) — the registry of agent tools.
//!
//! `GET /tools` lists every tool the agent registry exposes, as `{ name,
//! description }`, so the workbench can render a **checklist** when authoring an
//! agent profile's allowed-tool set (instead of free-typing names). The registry
//! is global/static — built once at boot, identical across workspaces (see
//! [`crate::state`]) — so the listing is workspace-independent.
//!
//! Authenticated like every REST surface, but it needs **no capability**: the
//! catalog is not sensitive, and a profile's `tools` list is only a *subset* hint
//! (empty = all) the runtime enforces per dispatch (SOUL §7) — it is never
//! validated against this listing. So `GET /tools` is open to any workspace
//! member, exactly like `GET /status`.

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use crate::auth::Auth;
use crate::error::ApiResult;
use crate::state::AppState;

/// Mount the tools route.
pub fn router() -> Router<AppState> {
    Router::new().route("/tools", get(list))
}

/// One agent tool in the catalog — the name a profile lists in its `tools` set
/// plus a one-line description for the picker. The full JSON-Schema `parameters`
/// the model is shown is deliberately omitted: a checklist needs only the name
/// and a blurb, and the schemas are large.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolInfo {
    /// Stable tool name (the value stored in a profile's `tools`).
    pub name: String,
    /// One-line description (may be empty for a terse tool).
    pub description: String,
}

/// `GET /tools` — the agent tool catalog, name-sorted for a stable picker.
async fn list(State(state): State<AppState>, _auth: Auth) -> ApiResult<Json<Vec<ToolInfo>>> {
    let mut tools: Vec<ToolInfo> = state
        .registry()
        .specs(None)
        .into_iter()
        .map(|s| ToolInfo {
            name: s.name,
            description: s.description,
        })
        .collect();
    // The registry iterates in hash order; sort so the picker is deterministic.
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Json(tools))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_info_serializes_name_and_description() {
        let json = serde_json::to_value(ToolInfo {
            name: "search".into(),
            description: "Search the web".into(),
        })
        .unwrap();
        assert_eq!(json["name"], "search");
        assert_eq!(json["description"], "Search the web");
    }
}
