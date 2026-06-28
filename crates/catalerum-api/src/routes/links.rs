//! Links REST (SOUL §5, §6.3) — user/agent-authored relationships between objects.
//!
//! A link is a **directed** `from → to` relationship between any two objects
//! ([`SourceRef`]s: files, notes, emails, calendar events, …) with an optional
//! free-text `label` and `note`. It is persisted in Postgres (the truth) and
//! projected into the derived Neo4j graph as a `RELATES_TO` edge (SOUL §6.3).
//!
//! All routes are workspace-scoped to the authenticated principal's workspace —
//! the client never names a workspace; cross-workspace reach is impossible by
//! construction (SOUL §18). They are **capability-gated** ([`Auth::require`],
//! SOUL §19): reads need `links:read` (every role), writes need `links:write`
//! (a Viewer is `403 Forbidden`). A link created here is authored by the calling
//! user ([`Author::User`]).
//!
//! Routes:
//! - `POST   /links`        create a link (`201`)
//! - `GET    /links`        list this workspace's links (most-recently-touched first)
//! - `GET    /links/for`    list every link touching an endpoint (`?kind=&id=`)
//! - `GET    /links/{id}`   fetch one link
//! - `DELETE /links/{id}`   delete a link (`204`)

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;

use catalerum_core::capability::Action;
use catalerum_core::model::{Author, Link, SourceRef};
use catalerum_core::LinkId;

use crate::auth::Auth;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Mount the links routes. `/links/for` is registered before `/links/{id}` so the
/// literal path wins over the `{id}` capture.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/links", get(list).post(create))
        .route("/links/for", get(list_for))
        .route("/links/{id}", get(get_one).delete(delete_link))
}

/// Body for `POST /links`. `from`/`to` are tagged [`SourceRef`]s
/// (`{"kind":"note","id":"…"}`); `label`/`note` are optional free text.
#[derive(Debug, Deserialize)]
pub struct CreateLink {
    /// The source endpoint.
    pub from: SourceRef,
    /// The target endpoint.
    pub to: SourceRef,
    /// Optional free-text relation label (e.g. "attachment", "follow-up").
    #[serde(default)]
    pub label: Option<String>,
    /// Optional annotation.
    #[serde(default)]
    pub note: Option<String>,
}

/// Query for `GET /links/for` — an endpoint named by its `SourceRef` parts, the
/// same `(kind, id)` split the store persists (e.g. `?kind=note&id=<uuid>`,
/// `?kind=external&id=<uri>`).
#[derive(Debug, Deserialize)]
pub struct ForQuery {
    /// The endpoint's kind discriminator (`note`/`event`/`object`/`email`/…).
    pub kind: String,
    /// The endpoint's id (a uuid for first-class rows, a uri for `external`).
    pub id: String,
}

/// Trim an optional free-text field to `None` when blank, so an empty string
/// isn't stored as a distinct value.
fn clean_opt(s: Option<String>) -> Option<String> {
    s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

async fn create(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<CreateLink>,
) -> ApiResult<(StatusCode, Json<Link>)> {
    auth.require(Action::Write, "links")?;
    let principal = auth.principal();
    let label = clean_opt(body.label);
    let note = clean_opt(body.note);
    // A self-link is rejected by the repository (a `400` via `StoreError::Invalid`).
    let link = state
        .store()
        .links()
        .create(
            principal.workspace_id,
            Author::User {
                id: principal.user_id,
            },
            &body.from,
            &body.to,
            label.as_deref(),
            note.as_deref(),
        )
        .await?;
    // Best-effort: project the `RELATES_TO` edge into the graph (SOUL §6.3). Never
    // fails the write; a no-op unless `[neo4j].enabled`.
    state
        .enqueue_link_projection(principal.workspace_id, link.id)
        .await;
    Ok((StatusCode::CREATED, Json(link)))
}

async fn list(State(state): State<AppState>, auth: Auth) -> ApiResult<Json<Vec<Link>>> {
    auth.require(Action::Read, "links")?;
    let ws = auth.principal().workspace_id;
    // §18: bounded to the most-recent `DEFAULT_LINK_LIMIT` so a huge link set
    // can't balloon the payload.
    let links = state
        .store()
        .links()
        .list_by_workspace(ws, catalerum_store::DEFAULT_LINK_LIMIT)
        .await?;
    Ok(Json(links))
}

async fn list_for(
    State(state): State<AppState>,
    auth: Auth,
    Query(q): Query<ForQuery>,
) -> ApiResult<Json<Vec<Link>>> {
    auth.require(Action::Read, "links")?;
    let ws = auth.principal().workspace_id;
    let endpoint = catalerum_store::source_from_parts(&q.kind, &q.id)
        .map_err(|e| ApiError::bad_request(format!("invalid endpoint: {e}")))?;
    let links = state
        .store()
        .links()
        .list_for(ws, &endpoint, catalerum_store::DEFAULT_LINK_LIMIT)
        .await?;
    Ok(Json(links))
}

async fn get_one(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<LinkId>,
) -> ApiResult<Json<Link>> {
    auth.require(Action::Read, "links")?;
    let ws = auth.principal().workspace_id;
    let link = state.store().links().get(ws, id).await?;
    Ok(Json(link))
}

async fn delete_link(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<LinkId>,
) -> ApiResult<StatusCode> {
    auth.require(Action::Write, "links")?;
    let ws = auth.principal().workspace_id;
    state.store().links().delete(ws, id).await?;
    // Reconcile the projection: the worker finds the link gone and purges its
    // edge (best-effort, no-op unless `[neo4j].enabled`).
    state.enqueue_link_projection(ws, id).await;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_link_parses_tagged_endpoints_and_optional_fields() {
        let body: CreateLink = serde_json::from_str(
            r#"{"from":{"kind":"note","id":"00000000-0000-0000-0000-000000000001"},
                "to":{"kind":"event","id":"00000000-0000-0000-0000-000000000002"},
                "label":"follow-up"}"#,
        )
        .unwrap();
        assert!(matches!(body.from, SourceRef::Note { .. }));
        assert!(matches!(body.to, SourceRef::Event { .. }));
        assert_eq!(body.label.as_deref(), Some("follow-up"));
        assert!(body.note.is_none());
    }

    #[test]
    fn clean_opt_blanks_to_none() {
        assert_eq!(clean_opt(Some("  ".into())), None);
        assert_eq!(clean_opt(Some(" hi ".into())), Some("hi".to_string()));
        assert_eq!(clean_opt(None), None);
    }
}
