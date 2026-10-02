//! Weekly-plan tools (SOUL §8): reusable week templates the agent can author,
//! edit, and apply to the calendar. Thin clients of [`WeeklyPlanRepo`] and the
//! shared [`crate::weekly_planner`] engine — the same code paths the
//! `/weekly-plans` REST routes use — gated on `calendar:read` / `calendar:write`.
//!
//! Plans are addressed **by name or id** (`plan`), entries by id. Entry times
//! are `HH:MM` wall-clock in the plan's timezone; weekdays accept `0`–`6`
//! (Monday = 0) or day names.
//!
//! [`WeeklyPlanRepo`]: catalerum_store::WeeklyPlanRepo

use super::*;

use catalerum_core::model::{WeeklyPlan, WeeklyPlanEntry};
use catalerum_core::{WeeklyPlanEntryId, WeeklyPlanId};
use catalerum_store::PlanInput;

use crate::weekly_planner::{
    apply_plan, ensure_target_calendar, format_hhmm, parse_timezone, parse_weekday, unapply_week,
    EntryDraft, WeekOutcome, MAX_APPLY_WEEKS, WEEKDAYS,
};

/// Resolve a `plan` argument — an id, or a plan name (case-insensitive) — to the
/// workspace's plan. Ambiguous or unknown names list the available plans.
async fn resolve_plan(store: &Store, ws: WorkspaceId, args: &Json) -> Result<WeeklyPlan> {
    let raw = required_str(args, "plan")?;
    let raw = raw.trim();
    if let Ok(id) = raw.parse::<WeeklyPlanId>() {
        return Ok(store.weekly_plans().get(ws, id).await?);
    }
    let plans = store.weekly_plans().list_by_workspace(ws).await?;
    let mut hits: Vec<WeeklyPlan> = plans
        .iter()
        .filter(|p| p.name.eq_ignore_ascii_case(raw))
        .cloned()
        .collect();
    match hits.len() {
        1 => Ok(hits.remove(0)),
        0 => {
            let names: Vec<&str> = plans.iter().map(|p| p.name.as_str()).collect();
            Err(Error::invalid(format!(
                "no weekly plan named `{raw}`; available: [{}]",
                names.join(", ")
            )))
        }
        _ => Err(Error::invalid(format!(
            "several weekly plans are named `{raw}`; pass the plan id instead"
        ))),
    }
}

/// A compact, model-friendly rendering of one entry.
fn entry_view(e: &WeeklyPlanEntry) -> Json {
    let mut v = json!({
        "id": e.id,
        "weekday": WEEKDAYS[e.weekday.clamp(0, 6) as usize],
        "summary": e.summary,
    });
    let map = v.as_object_mut().expect("object");
    if e.all_day {
        map.insert("all_day".into(), json!(true));
    } else {
        map.insert("start".into(), json!(format_hhmm(e.start_minute)));
        map.insert("end".into(), json!(format_hhmm(e.end_minute)));
    }
    if let Some(l) = &e.location {
        map.insert("location".into(), json!(l));
    }
    if let Some(b) = &e.body {
        map.insert("body".into(), json!(b));
    }
    if !e.labels.is_empty() {
        map.insert("labels".into(), json!(e.labels));
    }
    if let Some(c) = e.calendar_id {
        map.insert("calendar_id".into(), json!(c));
    }
    v
}

fn plan_view(p: &WeeklyPlan) -> Json {
    json!({
        "id": p.id,
        "name": p.name,
        "description": p.description,
        "timezone": p.timezone,
        "calendar_id": p.calendar_id,
        "entries": p.entries.iter().map(entry_view).collect::<Vec<_>>(),
    })
}

fn outcome_view(o: &WeekOutcome) -> Json {
    json!({
        "week_start": o.week_start,
        "created": o.created.len(),
        "updated": o.updated.len(),
        "unchanged": o.unchanged.len(),
        "removed": o.removed.len(),
    })
}

/// Parse a weekday argument given as a number or a day name.
fn weekday_arg(v: Option<&Json>) -> Result<Option<i32>> {
    match v {
        None | Some(Json::Null) => Ok(None),
        Some(Json::Number(n)) => n
            .as_i64()
            .and_then(|n| i32::try_from(n).ok())
            .filter(|n| (0..=6).contains(n))
            .map(Some)
            .ok_or_else(|| Error::invalid("`weekday` must be 0 (Monday) … 6 (Sunday)")),
        Some(Json::String(s)) => parse_weekday(s)
            .map(Some)
            .ok_or_else(|| Error::invalid(format!("unknown weekday `{s}`"))),
        Some(_) => Err(Error::invalid("`weekday` must be a number or a day name")),
    }
}

/// An optional calendar id field: absent → `None` (keep), `null`/`""` →
/// `Some(None)` (clear), a UUID → `Some(Some(id))`.
fn calendar_arg(obj: &Json, key: &str) -> Result<Option<Option<CalendarId>>> {
    match obj.get(key) {
        None => Ok(None),
        Some(Json::Null) => Ok(Some(None)),
        Some(Json::String(s)) if s.trim().is_empty() => Ok(Some(None)),
        Some(Json::String(s)) => s
            .trim()
            .parse::<CalendarId>()
            .map(|id| Some(Some(id)))
            .map_err(|e| Error::invalid(format!("invalid {key}: {e}"))),
        Some(_) => Err(Error::invalid(format!("`{key}` must be a calendar id string"))),
    }
}

fn str_field<'a>(obj: &'a Json, key: &str) -> Option<&'a str> {
    obj.get(key).and_then(Json::as_str)
}

fn labels_field(obj: &Json) -> Option<Vec<String>> {
    obj.get("labels").and_then(Json::as_array).map(|a| {
        a.iter()
            .filter_map(Json::as_str)
            .map(str::to_string)
            .collect()
    })
}

/// Build a draft from a full entry object (new entries).
fn new_entry_draft(obj: &Json) -> Result<EntryDraft> {
    let weekday = weekday_arg(obj.get("weekday"))?
        .ok_or_else(|| Error::invalid("each entry needs a `weekday`"))?;
    let calendar_id = calendar_arg(obj, "calendar_id")?.flatten();
    EntryDraft::new(
        weekday,
        str_field(obj, "start"),
        str_field(obj, "end"),
        obj.get("all_day").and_then(Json::as_bool).unwrap_or(false),
        str_field(obj, "summary").unwrap_or(""),
        str_field(obj, "location"),
        str_field(obj, "body"),
        &labels_field(obj).unwrap_or_default(),
        calendar_id,
    )
}

/// Build a draft by overlaying a partial entry object on an existing entry.
fn patched_entry_draft(existing: &WeeklyPlanEntry, obj: &Json) -> Result<EntryDraft> {
    let weekday = weekday_arg(obj.get("weekday"))?.unwrap_or(existing.weekday);
    let all_day = obj
        .get("all_day")
        .and_then(Json::as_bool)
        .unwrap_or(existing.all_day);
    let start = str_field(obj, "start")
        .map(str::to_string)
        .unwrap_or_else(|| format_hhmm(existing.start_minute));
    let end = str_field(obj, "end")
        .map(str::to_string)
        .unwrap_or_else(|| format_hhmm(existing.end_minute));
    // `""` clears an optional text field; absent keeps it.
    let keep = |key: &str, old: &Option<String>| -> Option<String> {
        match obj.get(key) {
            Some(Json::String(s)) => Some(s.clone()),
            Some(Json::Null) => None,
            _ => old.clone(),
        }
    };
    let calendar_id = match calendar_arg(obj, "calendar_id")? {
        Some(v) => v,
        None => existing.calendar_id,
    };
    EntryDraft::new(
        weekday,
        Some(&start),
        Some(&end),
        all_day,
        str_field(obj, "summary").unwrap_or(&existing.summary),
        keep("location", &existing.location).as_deref(),
        keep("body", &existing.body).as_deref(),
        &labels_field(obj).unwrap_or_else(|| existing.labels.clone()),
        calendar_id,
    )
}

/// JSON schema for one entry object; `required` lists the fields a *new*
/// entry needs.
fn entry_schema(with_id: bool) -> Json {
    let mut props = json!({
        "weekday": { "description": "Day of week: 0 (Monday) … 6 (Sunday), or a day name like \"wednesday\"." },
        "start": { "type": "string", "description": "Start time HH:MM (24h) in the plan's timezone. Omit for all-day." },
        "end": { "type": "string", "description": "End time HH:MM (24h, may be 24:00). Omit for all-day." },
        "all_day": { "type": "boolean", "description": "All-day entry (default false)." },
        "summary": { "type": "string", "description": "Event title." },
        "location": { "type": "string" },
        "body": { "type": "string", "description": "Description / notes." },
        "labels": { "type": "array", "items": { "type": "string" } },
        "calendar_id": { "type": "string", "description": "Optional per-entry calendar override (default: the plan's calendar)." }
    });
    let required = if with_id {
        props
            .as_object_mut()
            .expect("object")
            .insert("id".into(), json!({ "type": "string", "description": "Entry id to update." }));
        json!(["id"])
    } else {
        json!(["weekday", "summary"])
    };
    json!({ "type": "object", "properties": props, "required": required })
}

/// The acting user's profile timezone, when set and valid.
async fn profile_timezone(store: &Store, ctx: &ToolContext) -> Option<String> {
    let (ws, user) = (ctx.workspace_id?, ctx.user_id?);
    let profile = store.profiles().get(ws, user).await.ok()?;
    let tz = profile.fields.get("timezone")?.as_str()?;
    parse_timezone(tz).ok().map(|t| t.name().to_string())
}

/// Parse a `YYYY-MM-DD` date argument.
fn date_arg(args: &Json, key: &str) -> Result<chrono::NaiveDate> {
    let raw = required_str(args, key)?;
    chrono::NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d")
        .map_err(|_| Error::invalid(format!("`{key}` must be a date YYYY-MM-DD, got `{raw}`")))
}

/// `list_weekly_plans` — the workspace's weekly plans with their entries; with
/// `plan`, one plan plus the weeks it is applied to.
pub(crate) struct ListWeeklyPlansTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for ListWeeklyPlansTool {
    fn name(&self) -> &str {
        "list_weekly_plans"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Read, "calendar")
    }
    fn description(&self) -> &str {
        "List the workspace's weekly plans — reusable week templates (e.g. \"Normal \
         week\", \"Exam week\") whose entries (weekday + HH:MM–HH:MM) are applied to \
         concrete calendar weeks with apply_weekly_plan. Pass `plan` (name or id) to \
         get one plan plus the weeks it is currently applied to."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "plan": { "type": "string", "description": "Optional plan name or id to show in detail." }
            }
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        if args.get("plan").and_then(Json::as_str).is_some() {
            let plan = resolve_plan(&self.store, ws, &args).await?;
            let apps = self.store.weekly_plans().applications(ws, plan.id).await?;
            let mut v = plan_view(&plan);
            v.as_object_mut().expect("object").insert(
                "applied_weeks".into(),
                json!(apps
                    .iter()
                    .map(|a| json!({ "week_start": a.week_start, "events": a.event_count }))
                    .collect::<Vec<_>>()),
            );
            return Ok(v);
        }
        let plans = self.store.weekly_plans().list_by_workspace(ws).await?;
        Ok(json!({ "plans": plans.iter().map(plan_view).collect::<Vec<_>>() }))
    }
}

/// `create_weekly_plan` — author a new weekly plan, optionally with entries.
pub(crate) struct CreateWeeklyPlanTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for CreateWeeklyPlanTool {
    fn name(&self) -> &str {
        "create_weekly_plan"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "calendar")
    }
    fn description(&self) -> &str {
        "Create a weekly plan: a named, reusable week template of recurring slots \
         (weekday + HH:MM–HH:MM wall-clock, or all-day). It does NOT touch the \
         calendar until you apply it to a week with apply_weekly_plan. `timezone` \
         defaults to the user's profile timezone (else UTC); `calendar_id` is where \
         applied events go (default: the default local calendar). Returns the plan \
         with entry ids."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Plan name (e.g. \"Normal week\")." },
                "description": { "type": "string" },
                "timezone": { "type": "string", "description": "IANA timezone for the entry times (e.g. Europe/Berlin)." },
                "calendar_id": { "type": "string", "description": "Writable calendar applied events are written to." },
                "entries": { "type": "array", "items": entry_schema(false) }
            },
            "required": ["name"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let name = required_str(&args, "name")?;
        let name = name.trim();
        if name.is_empty() {
            return Err(Error::invalid("plan name must not be empty"));
        }
        let timezone = match opt_str_some(&args, "timezone") {
            Some(tz) => parse_timezone(&tz)?.name().to_string(),
            None => profile_timezone(&self.store, ctx)
                .await
                .unwrap_or_else(|| "UTC".to_string()),
        };
        let calendar_id = calendar_arg(&args, "calendar_id")?.flatten();
        ensure_target_calendar(&self.store, ws, calendar_id).await?;
        let drafts = args
            .get("entries")
            .and_then(Json::as_array)
            .map(|a| a.iter().map(new_entry_draft).collect::<Result<Vec<_>>>())
            .transpose()?
            .unwrap_or_default();
        for d in &drafts {
            ensure_target_calendar(&self.store, ws, d.calendar_id).await?;
        }
        let description = opt_str_some(&args, "description");
        let repo = self.store.weekly_plans();
        let plan = repo
            .create(
                ws,
                &PlanInput {
                    name,
                    description: description.as_deref(),
                    calendar_id,
                    timezone: &timezone,
                },
            )
            .await?;
        for d in &drafts {
            repo.add_entry(ws, plan.id, &d.as_input()).await?;
        }
        Ok(plan_view(&repo.get(ws, plan.id).await?))
    }
}

/// `edit_weekly_plan` — change a plan's header and add / update / remove
/// entries in one call.
pub(crate) struct EditWeeklyPlanTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for EditWeeklyPlanTool {
    fn name(&self) -> &str {
        "edit_weekly_plan"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "calendar")
    }
    fn description(&self) -> &str {
        "Edit a weekly plan (by name or id): rename it, change description / \
         timezone / calendar_id, and add, update, or remove entries in one call. \
         `update_entries` items are partial — only the given fields change. Edits \
         affect the calendar only when the plan is (re-)applied with \
         apply_weekly_plan, which then updates its previously applied events in \
         place. Returns the updated plan."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "plan": { "type": "string", "description": "Plan name or id." },
                "name": { "type": "string", "description": "New name." },
                "description": { "type": "string", "description": "New description (\"\" clears)." },
                "timezone": { "type": "string", "description": "New IANA timezone." },
                "calendar_id": { "type": "string", "description": "New target calendar (\"\" = default calendar)." },
                "add_entries": { "type": "array", "items": entry_schema(false) },
                "update_entries": { "type": "array", "items": entry_schema(true) },
                "remove_entries": { "type": "array", "items": { "type": "string" }, "description": "Entry ids to remove." }
            },
            "required": ["plan"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let plan = resolve_plan(&self.store, ws, &args).await?;
        let repo = self.store.weekly_plans();

        // Validate everything up front so a bad item doesn't leave a half edit.
        let adds = args
            .get("add_entries")
            .and_then(Json::as_array)
            .map(|a| a.iter().map(new_entry_draft).collect::<Result<Vec<_>>>())
            .transpose()?
            .unwrap_or_default();
        let mut updates = Vec::new();
        for obj in args
            .get("update_entries")
            .and_then(Json::as_array)
            .into_iter()
            .flatten()
        {
            let id: WeeklyPlanEntryId = parse_id(obj, "id")?;
            let existing = plan
                .entries
                .iter()
                .find(|e| e.id == id)
                .ok_or_else(|| Error::invalid(format!("plan has no entry {id}")))?;
            updates.push((id, patched_entry_draft(existing, obj)?));
        }
        let mut removals = Vec::new();
        for raw in opt_str_vec(&args, "remove_entries") {
            let id = raw
                .trim()
                .parse::<WeeklyPlanEntryId>()
                .map_err(|e| Error::invalid(format!("invalid entry id `{raw}`: {e}")))?;
            if !plan.entries.iter().any(|e| e.id == id) {
                return Err(Error::invalid(format!("plan has no entry {id}")));
            }
            removals.push(id);
        }
        for d in adds.iter().chain(updates.iter().map(|(_, d)| d)) {
            ensure_target_calendar(&self.store, ws, d.calendar_id).await?;
        }

        let header_touched = ["name", "description", "timezone", "calendar_id"]
            .iter()
            .any(|k| args.get(*k).is_some());
        if header_touched {
            let name = opt_str_some(&args, "name").unwrap_or_else(|| plan.name.clone());
            let description = match args.get("description") {
                Some(Json::String(s)) => Some(s.trim().to_string()).filter(|s| !s.is_empty()),
                Some(Json::Null) => None,
                _ => plan.description.clone(),
            };
            let timezone = match opt_str_some(&args, "timezone") {
                Some(tz) => parse_timezone(&tz)?.name().to_string(),
                None => plan.timezone.clone(),
            };
            let calendar_id = match calendar_arg(&args, "calendar_id")? {
                Some(v) => v,
                None => plan.calendar_id,
            };
            ensure_target_calendar(&self.store, ws, calendar_id).await?;
            repo.update(
                ws,
                plan.id,
                &PlanInput {
                    name: name.trim(),
                    description: description.as_deref(),
                    calendar_id,
                    timezone: &timezone,
                },
            )
            .await?;
        }
        for d in &adds {
            repo.add_entry(ws, plan.id, &d.as_input()).await?;
        }
        for (id, d) in &updates {
            repo.update_entry(ws, plan.id, *id, &d.as_input()).await?;
        }
        for id in removals {
            repo.delete_entry(ws, plan.id, id).await?;
        }
        Ok(plan_view(&repo.get(ws, plan.id).await?))
    }
}

/// `duplicate_weekly_plan` — copy a plan (entries included) under a new name,
/// e.g. to tweak next week without changing the regular template.
pub(crate) struct DuplicateWeeklyPlanTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for DuplicateWeeklyPlanTool {
    fn name(&self) -> &str {
        "duplicate_weekly_plan"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "calendar")
    }
    fn description(&self) -> &str {
        "Copy a weekly plan (by name or id) including all its entries under a new \
         name — e.g. start a one-off variant for next week from the regular plan, \
         edit the copy with edit_weekly_plan, then apply it. Returns the new plan."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "plan": { "type": "string", "description": "Plan name or id to copy." },
                "name": { "type": "string", "description": "Name of the copy (default: \"<name> (copy)\")." }
            },
            "required": ["plan"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let plan = resolve_plan(&self.store, ws, &args).await?;
        let name = opt_str_some(&args, "name").unwrap_or_else(|| format!("{} (copy)", plan.name));
        let copy = self
            .store
            .weekly_plans()
            .duplicate(ws, plan.id, name.trim())
            .await?;
        Ok(plan_view(&copy))
    }
}

/// `apply_weekly_plan` — materialise a plan into one or more calendar weeks.
pub(crate) struct ApplyWeeklyPlanTool {
    pub(crate) store: Store,
    pub(crate) ingest: NoteIngest,
    pub(crate) secrets: Option<Arc<catalerum_store::SecretStore>>,
}

#[async_trait]
impl Tool for ApplyWeeklyPlanTool {
    fn name(&self) -> &str {
        "apply_weekly_plan"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "calendar")
    }
    fn description(&self) -> &str {
        "Apply a weekly plan (by name or id) to the calendar week containing \
         `week_start` (YYYY-MM-DD, any day of that week; call current_time first to \
         resolve \"next week\"), optionally for several consecutive `weeks`. Creates \
         one event per entry, linked to the plan. Re-applying the same week is \
         safe: linked events are updated in place, events of removed entries are \
         deleted, and nothing is duplicated. Events added by hand are never \
         touched. Returns per-week counts."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "plan": { "type": "string", "description": "Plan name or id." },
                "week_start": { "type": "string", "description": "Any date (YYYY-MM-DD) in the first target week." },
                "weeks": { "type": "integer", "minimum": 1, "maximum": MAX_APPLY_WEEKS, "description": "Consecutive weeks to apply (default 1)." }
            },
            "required": ["plan", "week_start"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let plan = resolve_plan(&self.store, ws, &args).await?;
        let week = date_arg(&args, "week_start")?;
        let weeks = opt_clamped_u64(&args, "weeks", 1, u64::from(MAX_APPLY_WEEKS)) as u32;
        let outcomes =
            apply_plan(&self.store, self.secrets.as_ref(), ws, &plan, week, weeks.max(1)).await?;
        for id in outcomes.iter().flat_map(WeekOutcome::touched) {
            self.ingest.enqueue_event(ws, id).await;
        }
        Ok(json!({
            "plan": plan.name,
            "weeks": outcomes.iter().map(outcome_view).collect::<Vec<_>>(),
        }))
    }
}

/// `unapply_weekly_plan` — remove a plan's linked events from one week.
pub(crate) struct UnapplyWeeklyPlanTool {
    pub(crate) store: Store,
    pub(crate) ingest: NoteIngest,
    pub(crate) secrets: Option<Arc<catalerum_store::SecretStore>>,
}

#[async_trait]
impl Tool for UnapplyWeeklyPlanTool {
    fn name(&self) -> &str {
        "unapply_weekly_plan"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "calendar")
    }
    fn description(&self) -> &str {
        "Undo apply_weekly_plan for one week: delete exactly the events the plan \
         (by name or id) created in the week containing `week_start` (YYYY-MM-DD). \
         Other events in that week are untouched. Returns how many were removed."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "plan": { "type": "string", "description": "Plan name or id." },
                "week_start": { "type": "string", "description": "Any date (YYYY-MM-DD) in the target week." }
            },
            "required": ["plan", "week_start"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let plan = resolve_plan(&self.store, ws, &args).await?;
        let week = date_arg(&args, "week_start")?;
        let outcome = unapply_week(&self.store, self.secrets.as_ref(), ws, &plan, week).await?;
        for id in outcome.touched() {
            self.ingest.enqueue_event(ws, id).await;
        }
        Ok(json!({ "plan": plan.name, "week": outcome_view(&outcome) }))
    }
}

/// `delete_weekly_plan` — delete a plan template. Its applied events stay in
/// the calendar (unlinked).
pub(crate) struct DeleteWeeklyPlanTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for DeleteWeeklyPlanTool {
    fn name(&self) -> &str {
        "delete_weekly_plan"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "calendar")
    }
    fn description(&self) -> &str {
        "Delete a weekly plan (by name or id). Events it already applied stay in \
         the calendar as ordinary events — use unapply_weekly_plan first to remove \
         them from a week."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "plan": { "type": "string", "description": "Plan name or id." }
            },
            "required": ["plan"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let plan = resolve_plan(&self.store, ws, &args).await?;
        self.store.weekly_plans().delete(ws, plan.id).await?;
        Ok(json!({ "deleted": plan.id, "name": plan.name }))
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    fn existing() -> WeeklyPlanEntry {
        WeeklyPlanEntry {
            id: WeeklyPlanEntryId::new(),
            plan_id: WeeklyPlanId::new(),
            weekday: 1,
            start_minute: 540,
            end_minute: 600,
            all_day: false,
            summary: "Standup".into(),
            location: Some("Room 2".into()),
            body: None,
            labels: vec!["work".into()],
            calendar_id: None,
        }
    }

    #[test]
    fn weekday_arg_accepts_numbers_and_names() {
        assert_eq!(weekday_arg(Some(&json!(3))).unwrap(), Some(3));
        assert_eq!(weekday_arg(Some(&json!("Friday"))).unwrap(), Some(4));
        assert_eq!(weekday_arg(None).unwrap(), None);
        assert!(weekday_arg(Some(&json!(9))).is_err());
        assert!(weekday_arg(Some(&json!("someday"))).is_err());
    }

    #[test]
    fn new_entry_requires_weekday_and_validates_times() {
        let d = new_entry_draft(
            &json!({"weekday":"mon","start":"07:30","end":"08:00","summary":"Run"}),
        )
        .unwrap();
        assert_eq!((d.weekday, d.start_minute, d.end_minute), (0, 450, 480));
        assert!(new_entry_draft(&json!({"start":"07:30","end":"08:00","summary":"Run"})).is_err());
        assert!(new_entry_draft(&json!({"weekday":0,"start":"09:00","end":"08:00","summary":"x"}))
            .is_err());
    }

    #[test]
    fn patched_entry_keeps_unspecified_fields() {
        let e = existing();
        let d = patched_entry_draft(&e, &json!({"end":"10:30"})).unwrap();
        assert_eq!((d.weekday, d.start_minute, d.end_minute), (1, 540, 630));
        assert_eq!(d.summary, "Standup");
        assert_eq!(d.location.as_deref(), Some("Room 2"));
        assert_eq!(d.labels, vec!["work".to_string()]);

        let cleared = patched_entry_draft(&e, &json!({"location":"","weekday":"thu"})).unwrap();
        assert_eq!(cleared.location, None);
        assert_eq!(cleared.weekday, 3);
    }

    #[test]
    fn calendar_arg_distinguishes_absent_clear_and_set() {
        let id = CalendarId::new();
        assert_eq!(calendar_arg(&json!({}), "calendar_id").unwrap(), None);
        assert_eq!(
            calendar_arg(&json!({"calendar_id": ""}), "calendar_id").unwrap(),
            Some(None)
        );
        assert_eq!(
            calendar_arg(&json!({"calendar_id": id.to_string()}), "calendar_id").unwrap(),
            Some(Some(id))
        );
        assert!(calendar_arg(&json!({"calendar_id": "nope"}), "calendar_id").is_err());
    }

    #[test]
    fn entry_view_renders_names_and_times() {
        let v = entry_view(&existing());
        assert_eq!(v["weekday"], "tuesday");
        assert_eq!(v["start"], "09:00");
        assert_eq!(v["end"], "10:00");
    }
}
