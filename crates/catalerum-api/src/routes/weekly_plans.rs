//! Weekly plans REST (SOUL §8) — reusable week templates applied to calendars.
//!
//! A plan is a named set of slots (weekday + `HH:MM`–`HH:MM` in the plan's IANA
//! timezone). Plans are edited independently of the calendar; **applying** one
//! to a week materialises an event per slot and links each event back to its
//! plan + entry + week, so re-applying updates those events in place and
//! un-applying removes exactly them (see [`crate::weekly_planner`]).
//!
//! Workspace-scoped to the authenticated principal (SOUL §18) and
//! capability-gated on the `calendar` domain (SOUL §19): reads need
//! `calendar:read`; every mutation — including apply/unapply, which only ever
//! writes or removes the plan's *own* linked events — needs `calendar:write`.
//!
//! Routes:
//! - `GET    /weekly-plans`                           list plans (with entries)
//! - `POST   /weekly-plans`                           create (`{name, description?, calendar_id?, timezone?, entries?}`)
//! - `GET    /weekly-plans/{id}`                      one plan
//! - `PUT    /weekly-plans/{id}`                      edit header (`{name, description?, calendar_id?, timezone}`)
//! - `DELETE /weekly-plans/{id}`                      delete plan (applied events stay, unlinked; `204`)
//! - `POST   /weekly-plans/{id}/duplicate`            copy plan + entries (`{name?}`)
//! - `POST   /weekly-plans/{id}/entries`              add an entry
//! - `PUT    /weekly-plans/{id}/entries/{entry_id}`   replace an entry
//! - `DELETE /weekly-plans/{id}/entries/{entry_id}`   remove an entry (`204`)
//! - `GET    /weekly-plans/{id}/applications`         weeks the plan is applied to
//! - `POST   /weekly-plans/{id}/apply`                apply to week(s) (`{week_start, weeks?}`)
//! - `POST   /weekly-plans/{id}/unapply`              remove the plan's events from a week (`{week_start}`)
//! - `GET    /weekly-plan-links`                      event → plan links (for calendar badges)

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use chrono::NaiveDate;
use serde::Deserialize;

use catalerum_core::capability::Action;
use catalerum_core::model::{WeeklyPlan, WeeklyPlanApplication, WeeklyPlanEntry};
use catalerum_core::{CalendarId, WeeklyPlanEntryId, WeeklyPlanId};
use catalerum_store::{PlanEventLink, PlanInput, DEFAULT_PLAN_LINK_LIMIT};

use crate::auth::Auth;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use crate::weekly_planner::{
    apply_plan, ensure_target_calendar, parse_timezone, unapply_week, EntryDraft, WeekOutcome,
};

/// Mount the weekly-plan routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/weekly-plans", get(list_plans).post(create_plan))
        .route(
            "/weekly-plans/{id}",
            get(get_plan).put(update_plan).delete(delete_plan),
        )
        .route("/weekly-plans/{id}/duplicate", post(duplicate_plan))
        .route("/weekly-plans/{id}/entries", post(add_entry))
        .route(
            "/weekly-plans/{id}/entries/{entry_id}",
            put(update_entry).delete(delete_entry),
        )
        .route("/weekly-plans/{id}/applications", get(list_applications))
        .route("/weekly-plans/{id}/apply", post(apply))
        .route("/weekly-plans/{id}/unapply", post(unapply))
        .route("/weekly-plan-links", get(list_links))
}

/// One entry in a create/update body. Times are `HH:MM` wall-clock in the
/// plan's timezone; `start`/`end` may be omitted for an all-day entry.
#[derive(Debug, Deserialize)]
pub struct EntryBody {
    /// `0` = Monday … `6` = Sunday.
    pub weekday: i32,
    #[serde(default)]
    pub start: Option<String>,
    #[serde(default)]
    pub end: Option<String>,
    #[serde(default)]
    pub all_day: bool,
    pub summary: String,
    #[serde(default)]
    pub location: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    /// Per-entry calendar override; absent = the plan's calendar.
    #[serde(default)]
    pub calendar_id: Option<CalendarId>,
}

impl EntryBody {
    fn draft(&self) -> Result<EntryDraft, ApiError> {
        Ok(EntryDraft::new(
            self.weekday,
            self.start.as_deref(),
            self.end.as_deref(),
            self.all_day,
            &self.summary,
            self.location.as_deref(),
            self.body.as_deref(),
            &self.labels,
            self.calendar_id,
        )?)
    }
}

/// Body for `POST /weekly-plans`.
#[derive(Debug, Deserialize)]
pub struct CreatePlan {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub calendar_id: Option<CalendarId>,
    /// IANA timezone; defaults to `UTC`.
    #[serde(default)]
    pub timezone: Option<String>,
    /// Optional initial entries.
    #[serde(default)]
    pub entries: Vec<EntryBody>,
}

/// Body for `PUT /weekly-plans/{id}` — a full replace of the header fields.
#[derive(Debug, Deserialize)]
pub struct UpdatePlan {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub calendar_id: Option<CalendarId>,
    pub timezone: String,
}

/// Body for `POST /weekly-plans/{id}/duplicate`.
#[derive(Debug, Default, Deserialize)]
pub struct DuplicatePlan {
    #[serde(default)]
    pub name: Option<String>,
}

/// Body for `POST /weekly-plans/{id}/apply`.
#[derive(Debug, Deserialize)]
pub struct ApplyBody {
    /// Any date in the (first) target week; normalized to its Monday.
    pub week_start: NaiveDate,
    /// How many consecutive weeks to apply (default 1, max 12).
    #[serde(default)]
    pub weeks: Option<u32>,
}

/// Body for `POST /weekly-plans/{id}/unapply`.
#[derive(Debug, Deserialize)]
pub struct UnapplyBody {
    /// Any date in the target week; normalized to its Monday.
    pub week_start: NaiveDate,
}

fn clean_name(name: &str) -> Result<&str, ApiError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("plan name must not be empty"));
    }
    Ok(name)
}

fn clean_opt(v: Option<&String>) -> Option<&str> {
    v.map(|s| s.trim()).filter(|s| !s.is_empty())
}

/// Validate + normalize a timezone name (defaulting to UTC).
fn clean_timezone(tz: Option<&str>) -> Result<String, ApiError> {
    match tz.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok("UTC".to_string()),
        Some(name) => Ok(parse_timezone(name)?.name().to_string()),
    }
}

async fn load_plan(state: &AppState, auth: &Auth, id: WeeklyPlanId) -> ApiResult<WeeklyPlan> {
    let ws = auth.principal().workspace_id;
    state
        .store()
        .weekly_plans()
        .get(ws, id)
        .await
        .map_err(|_| ApiError::NotFound)
}

async fn list_plans(State(state): State<AppState>, auth: Auth) -> ApiResult<Json<Vec<WeeklyPlan>>> {
    auth.require(Action::Read, "calendar")?;
    let ws = auth.principal().workspace_id;
    Ok(Json(state.store().weekly_plans().list_by_workspace(ws).await?))
}

async fn get_plan(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<WeeklyPlanId>,
) -> ApiResult<Json<WeeklyPlan>> {
    auth.require(Action::Read, "calendar")?;
    Ok(Json(load_plan(&state, &auth, id).await?))
}

async fn create_plan(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<CreatePlan>,
) -> ApiResult<(StatusCode, Json<WeeklyPlan>)> {
    auth.require(Action::Write, "calendar")?;
    let ws = auth.principal().workspace_id;
    let name = clean_name(&body.name)?;
    let timezone = clean_timezone(body.timezone.as_deref())?;
    ensure_target_calendar(state.store(), ws, body.calendar_id).await?;
    // Validate every entry before writing anything.
    let drafts = body
        .entries
        .iter()
        .map(EntryBody::draft)
        .collect::<Result<Vec<_>, _>>()?;
    for d in &drafts {
        ensure_target_calendar(state.store(), ws, d.calendar_id).await?;
    }
    let repo = state.store().weekly_plans();
    let plan = repo
        .create(
            ws,
            &PlanInput {
                name,
                description: clean_opt(body.description.as_ref()),
                calendar_id: body.calendar_id,
                timezone: &timezone,
            },
        )
        .await?;
    for d in &drafts {
        repo.add_entry(ws, plan.id, &d.as_input()).await?;
    }
    let plan = repo.get(ws, plan.id).await?;
    Ok((StatusCode::CREATED, Json(plan)))
}

async fn update_plan(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<WeeklyPlanId>,
    Json(body): Json<UpdatePlan>,
) -> ApiResult<Json<WeeklyPlan>> {
    auth.require(Action::Write, "calendar")?;
    let ws = auth.principal().workspace_id;
    let name = clean_name(&body.name)?;
    let timezone = clean_timezone(Some(&body.timezone))?;
    ensure_target_calendar(state.store(), ws, body.calendar_id).await?;
    let plan = state
        .store()
        .weekly_plans()
        .update(
            ws,
            id,
            &PlanInput {
                name,
                description: clean_opt(body.description.as_ref()),
                calendar_id: body.calendar_id,
                timezone: &timezone,
            },
        )
        .await
        .map_err(|_| ApiError::NotFound)?;
    Ok(Json(plan))
}

async fn delete_plan(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<WeeklyPlanId>,
) -> ApiResult<StatusCode> {
    auth.require(Action::Write, "calendar")?;
    let ws = auth.principal().workspace_id;
    state
        .store()
        .weekly_plans()
        .delete(ws, id)
        .await
        .map_err(|_| ApiError::NotFound)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn duplicate_plan(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<WeeklyPlanId>,
    body: Option<Json<DuplicatePlan>>,
) -> ApiResult<(StatusCode, Json<WeeklyPlan>)> {
    auth.require(Action::Write, "calendar")?;
    let ws = auth.principal().workspace_id;
    let source = load_plan(&state, &auth, id).await?;
    let requested = body.and_then(|Json(b)| b.name);
    let name = match clean_opt(requested.as_ref()) {
        Some(n) => n.to_string(),
        None => format!("{} (copy)", source.name),
    };
    let plan = state
        .store()
        .weekly_plans()
        .duplicate(ws, id, &name)
        .await?;
    Ok((StatusCode::CREATED, Json(plan)))
}

async fn add_entry(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<WeeklyPlanId>,
    Json(body): Json<EntryBody>,
) -> ApiResult<(StatusCode, Json<WeeklyPlanEntry>)> {
    auth.require(Action::Write, "calendar")?;
    let ws = auth.principal().workspace_id;
    let draft = body.draft()?;
    load_plan(&state, &auth, id).await?;
    ensure_target_calendar(state.store(), ws, draft.calendar_id).await?;
    let entry = state
        .store()
        .weekly_plans()
        .add_entry(ws, id, &draft.as_input())
        .await?;
    Ok((StatusCode::CREATED, Json(entry)))
}

async fn update_entry(
    State(state): State<AppState>,
    auth: Auth,
    Path((id, entry_id)): Path<(WeeklyPlanId, WeeklyPlanEntryId)>,
    Json(body): Json<EntryBody>,
) -> ApiResult<Json<WeeklyPlanEntry>> {
    auth.require(Action::Write, "calendar")?;
    let ws = auth.principal().workspace_id;
    let draft = body.draft()?;
    ensure_target_calendar(state.store(), ws, draft.calendar_id).await?;
    let entry = state
        .store()
        .weekly_plans()
        .update_entry(ws, id, entry_id, &draft.as_input())
        .await
        .map_err(|_| ApiError::NotFound)?;
    Ok(Json(entry))
}

async fn delete_entry(
    State(state): State<AppState>,
    auth: Auth,
    Path((id, entry_id)): Path<(WeeklyPlanId, WeeklyPlanEntryId)>,
) -> ApiResult<StatusCode> {
    auth.require(Action::Write, "calendar")?;
    let ws = auth.principal().workspace_id;
    state
        .store()
        .weekly_plans()
        .delete_entry(ws, id, entry_id)
        .await
        .map_err(|_| ApiError::NotFound)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_applications(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<WeeklyPlanId>,
) -> ApiResult<Json<Vec<WeeklyPlanApplication>>> {
    auth.require(Action::Read, "calendar")?;
    let ws = auth.principal().workspace_id;
    load_plan(&state, &auth, id).await?;
    Ok(Json(state.store().weekly_plans().applications(ws, id).await?))
}

async fn apply(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<WeeklyPlanId>,
    Json(body): Json<ApplyBody>,
) -> ApiResult<Json<Vec<WeekOutcome>>> {
    auth.require(Action::Write, "calendar")?;
    let ws = auth.principal().workspace_id;
    let plan = load_plan(&state, &auth, id).await?;
    let outcomes = apply_plan(
        state.store(),
        state.secret_store(),
        ws,
        &plan,
        body.week_start,
        body.weeks.unwrap_or(1),
    )
    .await?;
    // Best-effort graph reconcile of every touched event (SOUL §6.3).
    for event_id in outcomes.iter().flat_map(WeekOutcome::touched) {
        state.enqueue_event_projection(ws, event_id).await;
    }
    Ok(Json(outcomes))
}

async fn unapply(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<WeeklyPlanId>,
    Json(body): Json<UnapplyBody>,
) -> ApiResult<Json<WeekOutcome>> {
    auth.require(Action::Write, "calendar")?;
    let ws = auth.principal().workspace_id;
    let plan = load_plan(&state, &auth, id).await?;
    let outcome =
        unapply_week(state.store(), state.secret_store(), ws, &plan, body.week_start).await?;
    for event_id in outcome.touched() {
        state.enqueue_event_projection(ws, event_id).await;
    }
    Ok(Json(outcome))
}

async fn list_links(
    State(state): State<AppState>,
    auth: Auth,
) -> ApiResult<Json<Vec<PlanEventLink>>> {
    auth.require(Action::Read, "calendar")?;
    let ws = auth.principal().workspace_id;
    Ok(Json(
        state
            .store()
            .weekly_plans()
            .list_event_links(ws, DEFAULT_PLAN_LINK_LIMIT)
            .await?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_plan_decodes_with_defaults_and_entries() {
        let body: CreatePlan = serde_json::from_str(
            r#"{"name":"Normal week","entries":[
                {"weekday":0,"start":"09:00","end":"09:15","summary":"Standup"},
                {"weekday":5,"all_day":true,"summary":"Hike","labels":["outdoors"]}
            ]}"#,
        )
        .unwrap();
        assert_eq!(body.name, "Normal week");
        assert!(body.timezone.is_none() && body.calendar_id.is_none());
        assert_eq!(body.entries.len(), 2);
        let standup = body.entries[0].draft().unwrap();
        assert_eq!((standup.start_minute, standup.end_minute), (540, 555));
        let hike = body.entries[1].draft().unwrap();
        assert!(hike.all_day);
        assert_eq!(hike.labels, vec!["outdoors".to_string()]);
    }

    #[test]
    fn invalid_entry_is_a_bad_request() {
        let body: EntryBody =
            serde_json::from_str(r#"{"weekday":0,"start":"10:00","end":"09:00","summary":"x"}"#)
                .unwrap();
        assert!(body.draft().is_err());
    }

    #[test]
    fn timezone_defaults_to_utc_and_rejects_unknown() {
        assert_eq!(clean_timezone(None).unwrap(), "UTC");
        assert_eq!(clean_timezone(Some("  ")).unwrap(), "UTC");
        assert_eq!(
            clean_timezone(Some("Europe/Berlin")).unwrap(),
            "Europe/Berlin"
        );
        assert!(clean_timezone(Some("Nowhere/Land")).is_err());
    }

    #[test]
    fn apply_body_decodes_date_and_optional_weeks() {
        let b: ApplyBody = serde_json::from_str(r#"{"week_start":"2026-10-07"}"#).unwrap();
        assert_eq!(b.week_start, NaiveDate::from_ymd_opt(2026, 10, 7).unwrap());
        assert!(b.weeks.is_none());
        let b: ApplyBody =
            serde_json::from_str(r#"{"week_start":"2026-10-05","weeks":4}"#).unwrap();
        assert_eq!(b.weeks, Some(4));
    }
}
