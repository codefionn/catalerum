//! Calendar + event tools (SOUL §8/§11).

use super::*;

/// `read_event` — read one calendar event's **full detail** by id (SOUL §7/§8):
/// the description/body, attendees, and recurrence that `query_structured`
/// (upcoming_events / events_in_range) summaries omit (those carry only
/// summary/time/location). The calendar counterpart to `read_note` /
/// `read_object` / `read_email`; gated `calendar:read`. NotFound never leaks
/// another tenant's event.
pub(crate) struct ReadEventTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for ReadEventTool {
    fn name(&self) -> &str {
        "read_event"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Read, "calendar")
    }
    fn description(&self) -> &str {
        "Read a calendar event's full details by its id (the `id` from \
         query_structured upcoming_events/events_in_range or from search_events): \
         description, attendees, recurrence, location, labels, attachments, and \
         start/end times."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Event id (a UUID from query_structured)." }
            },
            "required": ["id"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let id: EventId = parse_id(&args, "id")?;
        let e = self.store.events().get(ws, id).await?;
        Ok(json!({
            "id": e.id,
            "summary": e.summary,
            "start": e.start,
            "end": e.end,
            "location": e.location,
            "body": e.body,
            "attendees": e.attendees,
            "rrule": e.rrule,
            "labels": e.labels,
            "attachments": e.attachments,
        }))
    }
}

/// `search_events` (SOUL §7/§8) — literal substring search over calendar events'
/// summary / location / body / attendee names, across **all** dates: unlike
/// `query_structured`'s `upcoming_events` (now-forward) and `events_in_range`
/// (needs exact bounds), this finds an event by what it *says*, past ones
/// included — "when did I last meet Alice". Most-recent start first; optional
/// `from`/`to` still narrow the window. The calendar sibling of
/// `kanban_search_tasks` / `search_messages`; thin `EventRepo` client, gated on
/// `calendar:read`.
pub(crate) struct SearchEventsTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for SearchEventsTool {
    fn name(&self) -> &str {
        "search_events"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Read, "calendar")
    }
    fn description(&self) -> &str {
        "Find calendar events by the text in their title, location, description, \
         or attendee names — a literal, case-insensitive substring search over ALL \
         events, past and future alike. Use it to locate an event by what it says \
         (\"the kickoff meeting\", \"when did I last meet Alice\") — most recent \
         start time first, so the latest match leads. Optionally bound the window \
         with `from`/`to` (RFC 3339); to list events purely by date use \
         query_structured's upcoming_events / events_in_range instead. Each hit \
         gives the event id, summary, times, location, labels, and a match-centred \
         snippet of the description; read one in full with `read_event`."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Exact text to find in an event's title, location, description, or attendee names (case-insensitive substring)."
                },
                "from": { "type": "string", "description": "Optional lower bound on the event start, RFC 3339 / ISO-8601. Omit to search all past events too." },
                "to": { "type": "string", "description": "Optional upper bound on the event start, RFC 3339 / ISO-8601; must not precede `from`." },
                "limit": {
                    "type": "integer",
                    "description": "Max results to return (1-50, default 10).",
                    "minimum": 1,
                    "maximum": 50
                }
            },
            "required": ["query"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let query = required_str(&args, "query")?;
        let from = opt_rfc3339(&args, "from")?;
        let to = opt_rfc3339(&args, "to")?;
        if let (Some(from), Some(to)) = (from, to) {
            if to < from {
                return Err(Error::invalid("`to` must not precede `from`"));
            }
        }
        let limit = opt_clamped_u64(&args, "limit", 10, 50) as i64;
        let events = self
            .store
            .events()
            .search_in_workspace(ws, &query, DateRange { from, to }, limit)
            .await
            .map_err(|e| Error::provider(format!("event search failed: {e}")))?;
        let results: Vec<Json> = events
            .into_iter()
            .map(|e| {
                // A match-centred body excerpt (like kanban_search_tasks); the
                // summary/location/attendees are already in the summary fields.
                let snippet = e
                    .body
                    .as_deref()
                    .filter(|b| !b.is_empty())
                    .map(|b| match_snippet(b, &query, MESSAGE_SNIPPET_CHARS));
                let mut summary = event_summary(e);
                if let (Json::Object(map), Some(snippet)) = (&mut summary, snippet) {
                    map.insert("snippet".into(), json!(snippet));
                }
                summary
            })
            .collect();
        Ok(json!({ "results": results }))
    }
}

/// The stable `external_id` of the auto-provisioned default local calendar — the
/// one `create_event` writes to when the caller names no `calendar_id`. Get-or-
/// created idempotently per workspace (the local partial unique index).
pub(crate) const DEFAULT_LOCAL_CALENDAR: &str = "default";

/// `create_calendar` — get-or-create a **local** (database-native) calendar by
/// name (SOUL §8/§11): no provider connection, read-write, never synced. The
/// tool twin of `POST /calendars`, so a chat agent can set up a dedicated
/// calendar before pointing `create_event` (or a `WriteEvent` automation action)
/// at it. Idempotent by name (case-insensitive) — asking for the same name twice
/// returns the same calendar rather than a duplicate — via the shared
/// [`crate::action_runner::get_or_create_local_calendar_by_name`]. Always
/// registered — a thin `CalendarRepo` client — and gated on `calendar:write`.
pub(crate) struct CreateCalendarTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for CreateCalendarTool {
    fn name(&self) -> &str {
        "create_calendar"
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "calendar")
    }

    fn description(&self) -> &str {
        "Create a local calendar with the given name (or return the existing one \
         of that name — this is get-or-create, so calling it twice with the same \
         name does not make a duplicate). Local calendars are database-native (no \
         provider connection, always writable). Returns the calendar including \
         its id — pass that id as `calendar_id` to create_event, or to a \
         WriteEvent automation action, to write events into it. To find existing \
         calendars first, use query_structured's 'calendars' operation."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Human-readable calendar name (required, non-empty)." }
            },
            "required": ["name"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let name = required_str(&args, "name")?;
        // Get-or-create by name (shared with the REST `POST /calendars` handler
        // and a `WriteEvent` name redirect), so asking for the same calendar
        // name twice returns the same calendar instead of a duplicate. The auto
        // "default" calendar `create_event` ensures uses a stable reserved key.
        let calendar =
            crate::action_runner::get_or_create_local_calendar_by_name(&self.store, ws, &name)
                .await
                .map_err(Error::provider)?;
        Ok(serde_json::to_value(calendar)?)
    }
}

/// `create_event` — add an event to a local (database-native) calendar or a
/// **writable provider calendar** (CalDAV/Google/Outlook write-back, SOUL
/// §8/§11). This is the write path automations use (the `CreateEvent` action
/// maps to this tool), and it is also exposed to the chat agent. Always
/// registered — a thin `CalendarRepo`/`EventRepo` client — and gated on
/// `calendar:write` (deny-by-default per §19).
pub(crate) struct CreateEventTool {
    pub(crate) store: Store,
    pub(crate) ingest: NoteIngest,
    /// Powers provider write-back (`crate::calendar_writeback`): the OAuth token
    /// seams a Google/Outlook provider needs. `None` = no master key; provider
    /// writes on those backends then fail with the factory's clear error.
    pub(crate) secrets: Option<Arc<catalerum_store::SecretStore>>,
}

#[async_trait]
impl Tool for CreateEventTool {
    fn name(&self) -> &str {
        "create_event"
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "calendar")
    }

    fn description(&self) -> &str {
        "Create a calendar event. Give a summary plus RFC-3339 / ISO-8601 start \
         and end times (e.g. 2026-06-18T09:00:00Z). Optionally set location, \
         body, all_day, labels (category tags), attachments (file/image links), \
         or a specific calendar_id; with no calendar_id a default local calendar \
         is created/used. A writable synced calendar (CalDAV, Google, Outlook) \
         is written back to the provider; read-only subscriptions are refused. \
         Returns the stored event including its id."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "summary": { "type": "string", "description": "Event title (required, non-empty)." },
                "start": { "type": "string", "description": "Start time, RFC 3339 / ISO-8601 (e.g. 2026-06-18T09:00:00Z)." },
                "end": { "type": "string", "description": "End time, RFC 3339 / ISO-8601; must not precede start." },
                "all_day": { "type": "boolean", "description": "All-day event. Optional, default false." },
                "location": { "type": "string", "description": "Optional location." },
                "body": { "type": "string", "description": "Optional description / notes." },
                "labels": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional category labels / tags (e.g. [\"work\", \"travel\"])."
                },
                "attachments": attachment_items_schema(),
                "calendar_id": { "type": "string", "description": "Optional id of a local calendar to add to; omit to use the default." }
            },
            "required": ["summary", "start", "end"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let summary = required_str(&args, "summary")?;
        // Parse keeping the input offset, then pin all-day endpoints to midnight
        // UTC of the written date (see `normalize_event_span`) so an all-day
        // event never renders a day early in the week/month grids.
        let start_in = required_rfc3339_offset(&args, "start")?;
        let end_in = required_rfc3339_offset(&args, "end")?;
        let all_day = args.get("all_day").and_then(Json::as_bool).unwrap_or(false);
        let (start, end) =
            crate::calendar_writeback::normalize_event_span(start_in, end_in, all_day);
        if end < start {
            return Err(Error::invalid("`end` must not precede `start`"));
        }
        let location = opt_str_some(&args, "location");
        let body = opt_str_some(&args, "body");
        let labels = parse_labels_arg(&args);
        let attachments = parse_attachments_arg(&args);

        // Resolve the target calendar: a named one, else the workspace's default
        // local calendar (created on first use). A writable provider calendar
        // routes through write-back; a read-only one is refused.
        let calendar = match args.get("calendar_id").and_then(Json::as_str) {
            Some(raw) => {
                let id = raw
                    .parse::<CalendarId>()
                    .map_err(|e| Error::invalid(format!("invalid calendar_id: {e}")))?;
                self.store.calendars().get(ws, id).await?
            }
            None => {
                self.store
                    .calendars()
                    .upsert_local(ws, DEFAULT_LOCAL_CALENDAR, "Calendar")
                    .await?
            }
        };
        let target = crate::calendar_writeback::resolve_event_write_target(
            &self.store,
            self.secrets.as_ref(),
            ws,
            calendar,
        )
        .await?;

        let event = match target {
            crate::calendar_writeback::EventWriteTarget::Local(calendar) => {
                let uid = uuid::Uuid::new_v4().to_string();
                self.store
                    .events()
                    .create(&UpsertEvent {
                        workspace_id: ws,
                        calendar_id: calendar.id,
                        uid: &uid,
                        starts_at: start,
                        ends_at: end,
                        all_day,
                        rrule: None,
                        summary: summary.trim(),
                        location: location.as_deref(),
                        body: body.as_deref(),
                        attendees: &[],
                        labels: &labels,
                        attachments: &attachments,
                        etag: None,
                        sequence: 0,
                    })
                    .await?
            }
            // Provider write-back (SOUL §8): the provider mints the uid/ETag;
            // the store mirrors its canonical event.
            crate::calendar_writeback::EventWriteTarget::Provider { calendar, provider } => {
                crate::calendar_writeback::create_on_provider(
                    &self.store,
                    &calendar,
                    &provider,
                    catalerum_core::provider::NewEvent {
                        summary: summary.trim().to_string(),
                        start,
                        end,
                        all_day,
                        location: location.clone(),
                        body: body.clone(),
                        rrule: None,
                        attendees: Vec::new(),
                        labels: labels.clone(),
                        attachments: attachments.clone(),
                    },
                )
                .await?
            }
        };
        // Best-effort graph projection (label topics + SCHEDULED_IN), matching the
        // REST create path; a no-op when no graph is configured.
        self.ingest.enqueue_event(ws, event.id).await;
        Ok(serde_json::to_value(event)?)
    }
}

/// JSON-schema fragment for an array of attachment descriptors, shared by the
/// `create_event` / `update_event` tools.
pub(crate) fn attachment_items_schema() -> Json {
    json!({
        "type": "array",
        "description": "Optional file / image attachments. Each is an object with a `url` (required) plus optional `filename` and `content_type`.",
        "items": {
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "Where the file lives: an absolute URL or a /storage/objects/{key} path." },
                "filename": { "type": "string", "description": "Optional display filename." },
                "content_type": { "type": "string", "description": "Optional MIME type (e.g. image/png)." }
            },
            "required": ["url"]
        }
    })
}

/// Parse a tool's `labels` array argument into a clean label list: trim, drop
/// blanks, dedup case-insensitively (first-seen casing kept). Matches the REST
/// `clean_labels` so both write paths store labels identically.
pub(crate) fn parse_labels_arg(args: &Json) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    args.get("labels")
        .and_then(Json::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Json::as_str)
                .filter_map(|s| {
                    let v = s.trim();
                    (!v.is_empty() && seen.insert(v.to_lowercase())).then(|| v.to_string())
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse a tool's `attachments` array argument into clean [`Attachment`]s:
/// drop any with a blank `url`, trim string fields. Matches the REST
/// `clean_attachments`.
pub(crate) fn parse_attachments_arg(args: &Json) -> Vec<Attachment> {
    args.get("attachments")
        .and_then(Json::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|a| {
                    let url = a.get("url").and_then(Json::as_str).unwrap_or("").trim();
                    if url.is_empty() {
                        return None;
                    }
                    let field = |k: &str| {
                        a.get(k)
                            .and_then(Json::as_str)
                            .map(str::trim)
                            .filter(|v| !v.is_empty())
                            .map(str::to_string)
                    };
                    Some(Attachment {
                        url: url.to_string(),
                        filename: field("filename"),
                        content_type: field("content_type"),
                        size: a.get("size").and_then(Json::as_u64),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `update_event` — edit an existing event on a local (database-native) calendar
/// in place (SOUL §8/§11). The write path the `UpdateEvent` automation action maps
/// to (and exposed to the chat agent); mirrors the `PUT /events/{id}` REST contract
/// — a **full replace** of the editable fields (summary/start/end required; absent
/// location/body/rrule are cleared), restricted to **local** calendars (a provider
/// calendar is managed by sync, never edited in place). Gated on `calendar:write`.
pub(crate) struct UpdateEventTool {
    pub(crate) store: Store,
    pub(crate) ingest: NoteIngest,
    /// Provider write-back seams (see [`CreateEventTool::secrets`]).
    pub(crate) secrets: Option<Arc<catalerum_store::SecretStore>>,
}

#[async_trait]
impl Tool for UpdateEventTool {
    fn name(&self) -> &str {
        "update_event"
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "calendar")
    }

    fn description(&self) -> &str {
        "Edit an existing calendar event. Give the event_id plus the new summary \
         and RFC-3339 / ISO-8601 start and end times (e.g. 2026-06-18T09:00:00Z); \
         this replaces the event's fields, so pass the full intended state. \
         Optionally set all_day, location, body, rrule, labels, or attachments — \
         any you omit are cleared. An event on a writable synced calendar \
         (CalDAV, Google, Outlook) is edited on the provider too; read-only \
         subscriptions are refused. Returns the updated event."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "event_id": { "type": "string", "description": "Id of the event to edit (must live on a local calendar)." },
                "summary": { "type": "string", "description": "New event title (required, non-empty)." },
                "start": { "type": "string", "description": "New start time, RFC 3339 / ISO-8601 (e.g. 2026-06-18T09:00:00Z)." },
                "end": { "type": "string", "description": "New end time, RFC 3339 / ISO-8601; must not precede start." },
                "all_day": { "type": "boolean", "description": "All-day event. Optional, default false." },
                "location": { "type": "string", "description": "Optional location; cleared when omitted." },
                "body": { "type": "string", "description": "Optional description / notes; cleared when omitted." },
                "rrule": { "type": "string", "description": "Optional RFC 5545 recurrence rule; cleared when omitted." },
                "labels": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "New category labels / tags; cleared when omitted."
                },
                "attachments": attachment_items_schema()
            },
            "required": ["event_id", "summary", "start", "end"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let event_id: EventId = parse_id(&args, "event_id")?;
        let summary = required_str(&args, "summary")?;
        // Offset-preserving parse + all-day midnight-UTC pinning, as in `create`.
        let start_in = required_rfc3339_offset(&args, "start")?;
        let end_in = required_rfc3339_offset(&args, "end")?;
        let all_day = args.get("all_day").and_then(Json::as_bool).unwrap_or(false);
        let (start, end) =
            crate::calendar_writeback::normalize_event_span(start_in, end_in, all_day);
        if end < start {
            return Err(Error::invalid("`end` must not precede `start`"));
        }
        let location = opt_str_some(&args, "location");
        let body = opt_str_some(&args, "body");
        let rrule = opt_str_some(&args, "rrule");
        let labels = parse_labels_arg(&args);
        let attachments = parse_attachments_arg(&args);

        // The event must exist in this workspace and sit on a writable calendar
        // before we touch it; a writable provider calendar routes through
        // write-back.
        let existing = self.store.events().get(ws, event_id).await?;
        let calendar = self.store.calendars().get(ws, existing.calendar_id).await?;
        let target = crate::calendar_writeback::resolve_event_write_target(
            &self.store,
            self.secrets.as_ref(),
            ws,
            calendar,
        )
        .await?;

        let event = match target {
            crate::calendar_writeback::EventWriteTarget::Local(_) => {
                self.store
                    .events()
                    .update(
                        ws,
                        event_id,
                        &EventPatch {
                            starts_at: start,
                            ends_at: end,
                            all_day,
                            summary: summary.trim(),
                            location: location.as_deref(),
                            body: body.as_deref(),
                            labels: &labels,
                            attachments: &attachments,
                            rrule: rrule.as_deref(),
                        },
                    )
                    .await?
            }
            // Provider write-back: push the merged full state, mirror the result.
            crate::calendar_writeback::EventWriteTarget::Provider { provider, .. } => {
                let merged = crate::calendar_writeback::merge_event_update(
                    &existing,
                    summary.trim(),
                    start,
                    end,
                    all_day,
                    location.as_deref(),
                    body.as_deref(),
                    rrule.as_deref(),
                    labels,
                    attachments,
                );
                crate::calendar_writeback::update_on_provider(&self.store, &provider, &merged)
                    .await?
            }
        };
        // Re-project so the graph reflects the edited summary/location/labels.
        self.ingest.enqueue_event(ws, event.id).await;
        Ok(serde_json::to_value(event)?)
    }
}

/// `delete_event` — remove a calendar event by id (SOUL §8). Completes the
/// calendar CRUD set with `create_event` / `update_event`; an event on a
/// writable provider calendar is deleted on the provider too (write-back).
/// Gated on `calendar:delete` (deny-by-default per §19 — no base role holds
/// it, so it mirrors the `DELETE /events` route's authority).
pub(crate) struct DeleteEventTool {
    pub(crate) store: Store,
    pub(crate) ingest: NoteIngest,
    /// Provider write-back seams (see [`CreateEventTool::secrets`]).
    pub(crate) secrets: Option<Arc<catalerum_store::SecretStore>>,
}

#[async_trait]
impl Tool for DeleteEventTool {
    fn name(&self) -> &str {
        "delete_event"
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Delete, "calendar")
    }

    fn description(&self) -> &str {
        "Delete a calendar event by event_id. An event on a writable synced \
         calendar (CalDAV, Google, Outlook) is deleted on the provider too; \
         read-only subscriptions are refused. Returns the deleted event's id."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "event_id": { "type": "string", "description": "Id of the event to delete (must live on a local calendar)." }
            },
            "required": ["event_id"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let event_id: EventId = parse_id(&args, "event_id")?;
        // The event must exist in this workspace and sit on a writable calendar;
        // a writable provider calendar routes through write-back.
        let existing = self.store.events().get(ws, event_id).await?;
        let calendar = self.store.calendars().get(ws, existing.calendar_id).await?;
        match crate::calendar_writeback::resolve_event_write_target(
            &self.store,
            self.secrets.as_ref(),
            ws,
            calendar,
        )
        .await?
        {
            crate::calendar_writeback::EventWriteTarget::Local(_) => {
                self.store.events().delete(ws, event_id).await?;
            }
            // Provider write-back: delete there first (idempotent on
            // already-gone), then drop the local mirror.
            crate::calendar_writeback::EventWriteTarget::Provider { provider, .. } => {
                crate::calendar_writeback::delete_on_provider(
                    &self.store,
                    ws,
                    &provider,
                    &existing,
                )
                .await?;
            }
        }
        // Reconcile the projection to a purge (worker finds the event gone).
        self.ingest.enqueue_event(ws, event_id).await;
        Ok(json!({ "deleted": event_id }))
    }
}
