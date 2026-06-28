//! `query_structured` — the typed multi-entity query tool.

use super::*;

/// `query_structured` — typed, **read-only**, workspace-scoped lookups over the
/// Postgres source of truth (SOUL §6.5/§7): the precise, non-fuzzy queries that
/// complement `search_semantic` (meaning) and `query_graph` (relationships). The
/// model never writes SQL — it picks a named `operation` and the tool runs the
/// matching typed repository query. Registered store-only in `build_registry`
/// (always available), then **re-registered from `AppState` with the storage
/// registry** so the object operations resolve each bucket's `?store=` name —
/// object labels key on it (SOUL §9); `None` falls back to connection names
/// (exactly right for runtime stores, and all there is without a registry).
pub(crate) struct QueryStructuredTool {
    pub(crate) store: Store,
    pub(crate) storage: Option<StorageRegistry>,
}

/// Replace the store-only `query_structured` from [`build_registry`] with one
/// holding the [`StorageRegistry`] (see the struct doc). Called from `AppState`
/// once the registry exists — after `build_registry`, like the other
/// storage-aware tools.
pub(crate) fn register_query_structured(
    registry: &mut ToolRegistry,
    store: Store,
    storage: StorageRegistry,
) {
    registry.register(Arc::new(QueryStructuredTool {
        store,
        storage: Some(storage),
    }));
}

#[async_trait]
impl Tool for QueryStructuredTool {
    fn name(&self) -> &str {
        "query_structured"
    }

    fn required_capability(&self) -> Option<Capability> {
        // This tool spans several domains (notes / calendar / tasks / storage), so
        // a single dispatch-level gate would either over- or under-authorize a
        // scoped caller. Capability is enforced **per operation** in `invoke`
        // against the op's own domain (the `UseSkillTool` pattern, §19).
        None
    }

    fn description(&self) -> &str {
        "Look up structured records precisely (not by meaning). operation = \
         'recent_notes' (most recently edited notes); 'notes_by_tag' (notes \
         carrying `tag`); 'upcoming_events' (calendar events from now forward); \
         'events_in_range' (calendar events between `from` and `to`, RFC 3339) — \
         both event ops list by *date*; to find events by their text (including \
         past ones) use `search_events` instead; \
         'open_tasks' (Kanban tasks not yet done); 'tasks_by_status' (tasks with \
         the given `status`: open | in_progress | blocked | done); 'tasks_by_board' \
         (every task on the named `board`, any status); 'boards' (your Kanban \
         boards with their columns — the kanban_* write tools also take board/column \
         *names* directly, so you rarely need the ids); 'calendars' (your \
         calendars with a `writable` flag — use it to find a calendar_id for \
         create_event); \
         'recent_objects' (most recently modified stored files); 'objects_by_prefix' \
         (stored files whose key starts with `prefix`); 'unlabeled_objects' (stored \
         files carrying no labels yet — filtered server-side so old unlabelled files \
         are reachable, optionally narrowed to a subdirectory via `prefix`; feed a \
         label-the-backlog sweep from this). Task results carry their board + column \
         names; object results carry their bucket + store + key + size + labels."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": [
                        "recent_notes", "notes_by_tag", "upcoming_events", "events_in_range",
                        "open_tasks", "tasks_by_status", "tasks_by_board", "boards",
                        "calendars", "recent_objects", "objects_by_prefix", "unlabeled_objects"
                    ],
                    "description": "Which structured query to run."
                },
                "tag": { "type": "string", "description": "Tag to filter by (required for notes_by_tag)." },
                "board": { "type": "string", "description": "Board name to filter by (required for tasks_by_board)." },
                "from": { "type": "string", "description": "Range start, RFC 3339 / ISO-8601 (required for events_in_range)." },
                "to": { "type": "string", "description": "Range end, RFC 3339 / ISO-8601; must not precede `from` (required for events_in_range)." },
                "prefix": { "type": "string", "description": "Key prefix to filter by (required for objects_by_prefix; optional for unlabeled_objects to restrict the sweep to one subdirectory, e.g. 'docs/')." },
                "status": {
                    "type": "string",
                    "enum": ["open", "in_progress", "blocked", "done"],
                    "description": "Task status to filter by (required for tasks_by_status)."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max results (1-50, default 10).",
                    "minimum": 1,
                    "maximum": 50
                }
            },
            "required": ["operation"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let limit = opt_clamped_u64(&args, "limit", 10, 50) as usize;
        let operation = required_str(&args, "operation")?;

        // Per-domain capability gate (§19): each op reads one domain, so a scoped
        // caller must hold *that* domain's read — a `notes:read`-only grant can't
        // reach calendar/tasks/storage data through this tool. Skipped when the
        // caller is unscoped (`capabilities == None`); an unknown op (domain None)
        // falls through to the operation `match`'s own error below.
        let domain = match operation.as_str() {
            "recent_notes" | "notes_by_tag" => Some("notes"),
            "upcoming_events" | "events_in_range" | "calendars" => Some("calendar"),
            "open_tasks" | "tasks_by_status" | "tasks_by_board" | "boards" => Some("tasks"),
            "recent_objects" | "objects_by_prefix" | "unlabeled_objects" => Some("storage"),
            _ => None,
        };
        if let (Some(caps), Some(domain)) = (&ctx.capabilities, domain) {
            let required = Capability::new(Action::Read, Resource::domain(domain));
            if !caps.iter().any(|held| held.covers(&required)) {
                return Err(Error::unauthorized(format!(
                    "query_structured `{operation}` requires {domain}:read which the caller's grant does not cover"
                )));
            }
        }

        let results: Vec<Json> = match operation.as_str() {
            "recent_notes" => {
                // Bound the fetch directly to the asked-for `limit` (newest-first).
                let notes = self
                    .store
                    .notes()
                    .list_by_workspace(ws, limit as i64)
                    .await
                    .map_err(query_err)?;
                notes.into_iter().map(note_summary).collect()
            }
            "notes_by_tag" => {
                let tag = required_str(&args, "tag")?;
                // Tag filtering is client-side over the most-recent `DEFAULT_NOTE_LIMIT`
                // notes (best-effort after the §18 bound, like recent_objects' prefix).
                let notes = self
                    .store
                    .notes()
                    .list_by_workspace(ws, catalerum_store::DEFAULT_NOTE_LIMIT)
                    .await
                    .map_err(query_err)?;
                notes
                    .into_iter()
                    .filter(|n| n.tags.iter().any(|t| t.eq_ignore_ascii_case(&tag)))
                    .take(limit)
                    .map(note_summary)
                    .collect()
            }
            "upcoming_events" => {
                let now = chrono::Utc::now();
                let events = self
                    .store
                    .events()
                    .list_by_workspace(
                        ws,
                        None,
                        DateRange {
                            from: Some(now),
                            to: None,
                        },
                        // Ascending from now → the repo's first `limit` are the
                        // soonest, so the bound and the `take` agree.
                        limit as i64,
                    )
                    .await
                    .map_err(query_err)?;
                events.into_iter().take(limit).map(event_summary).collect()
            }
            "events_in_range" => {
                // A bounded window (both ends inclusive on the repo side), unlike
                // `upcoming_events`' open-ended "now forward".
                let from = required_rfc3339(&args, "from")?;
                let to = required_rfc3339(&args, "to")?;
                if to < from {
                    return Err(Error::invalid("`to` must not precede `from`"));
                }
                let events = self
                    .store
                    .events()
                    .list_by_workspace(
                        ws,
                        None,
                        DateRange {
                            from: Some(from),
                            to: Some(to),
                        },
                        // First `limit` in the window, ascending — matches `take`.
                        limit as i64,
                    )
                    .await
                    .map_err(query_err)?;
                events.into_iter().take(limit).map(event_summary).collect()
            }
            "open_tasks" | "tasks_by_status" | "tasks_by_board" => {
                // `tasks_by_status` filters to one status; `open_tasks` is the
                // common "everything not Done" case (open + in_progress + blocked);
                // `tasks_by_board` is every task on one named board, any status.
                let wanted = if operation == "tasks_by_status" {
                    Some(parse_task_status(&required_str(&args, "status")?)?)
                } else {
                    None
                };
                // Index column_id → (board, column) names so each task carries
                // where it lives, not opaque ids.
                let boards = self
                    .store
                    .boards()
                    .list_by_workspace(ws)
                    .await
                    .map_err(query_err)?;
                // For `tasks_by_board`, resolve the named board → its id
                // (case-insensitive); an unknown name is an error, not a silent
                // empty result.
                let board_filter: Option<BoardId> = if operation == "tasks_by_board" {
                    let name = required_str(&args, "board")?;
                    Some(
                        boards
                            .iter()
                            .find(|b| b.name.eq_ignore_ascii_case(name.trim()))
                            .map(|b| b.id)
                            .ok_or_else(|| Error::invalid(format!("no board named `{name}`")))?,
                    )
                } else {
                    None
                };
                let mut col_index: std::collections::HashMap<ColumnId, (String, String)> =
                    std::collections::HashMap::new();
                for b in &boards {
                    for c in &b.columns {
                        col_index.insert(c.id, (b.name.clone(), c.name.clone()));
                    }
                }
                let tasks = self
                    .store
                    .tasks()
                    .list_by_workspace(ws)
                    .await
                    .map_err(query_err)?;
                tasks
                    .into_iter()
                    .filter(|t| board_filter.is_none_or(|bid| t.board_id == bid))
                    .filter(|t| match wanted {
                        Some(s) => t.status == s,
                        None if operation == "open_tasks" => t.status != TaskStatus::Done,
                        None => true,
                    })
                    .take(limit)
                    .map(|t| task_summary(t, &col_index))
                    .collect()
            }
            "boards" => {
                // Enumerate the workspace's boards with their columns (names + ids).
                // The kanban_* write tools take board/column *names* directly, so
                // this is mainly a discovery read — what boards/columns exist — and
                // the id escape hatch for duplicate names.
                let boards = self
                    .store
                    .boards()
                    .list_by_workspace(ws)
                    .await
                    .map_err(query_err)?;
                boards
                    .into_iter()
                    .take(limit)
                    .map(|b| {
                        json!({
                            "id": b.id,
                            "name": b.name,
                            "columns": b
                                .columns
                                .iter()
                                .map(|c| json!({ "id": c.id, "name": c.name }))
                                .collect::<Vec<_>>(),
                        })
                    })
                    .collect()
            }
            "calendars" => {
                // The workspace's calendars, each flagged `writable` (a local,
                // non-read-only calendar — the only kind create_event/update_event
                // accept) so the agent can pick a valid `calendar_id`.
                let calendars = self
                    .store
                    .calendars()
                    .list_by_workspace(ws)
                    .await
                    .map_err(query_err)?;
                calendars
                    .into_iter()
                    .take(limit)
                    .map(|c| {
                        json!({
                            "id": c.id,
                            "name": c.name,
                            "read_only": c.read_only,
                            "local": c.is_local(),
                            "writable": c.is_local() && !c.read_only,
                        })
                    })
                    .collect()
            }
            "recent_objects" | "objects_by_prefix" | "unlabeled_objects" => {
                // `objects_by_prefix` filters to keys under `prefix` (required);
                // `unlabeled_objects` takes it as an optional subdirectory bound;
                // `recent_objects` is everything, newest-modified first.
                let prefix = match operation.as_str() {
                    "objects_by_prefix" => required_str(&args, "prefix")?,
                    "unlabeled_objects" => opt_str(&args, "prefix"),
                    _ => String::new(),
                };
                // Index bucket_id → (bucket name, store name) so each object
                // carries where it lives, not an opaque id — the store name is
                // what object labels key on (SOUL §9).
                let bucket_index = bucket_store_map(&self.store, self.storage.as_ref(), ws).await?;
                // Prefix + bound are applied in SQL (the bound after the filter),
                // so the soonest `limit` matches return without an unbounded pull;
                // `unlabeled_objects` additionally anti-joins the labels there, so
                // a backlog sweep reaches old unlabelled files even when every
                // recent one is labelled.
                let objects = if operation == "unlabeled_objects" {
                    let bucket_stores: Vec<(BucketId, String)> = bucket_index
                        .iter()
                        .map(|(id, (_, store))| (*id, store.clone()))
                        .collect();
                    self.store
                        .objects()
                        .list_unlabeled_by_workspace(ws, &bucket_stores, &prefix, limit as i64)
                        .await
                        .map_err(query_err)?
                } else {
                    self.store
                        .objects()
                        .list_by_workspace(ws, &prefix, limit as i64)
                        .await
                        .map_err(query_err)?
                };
                // ONE batched label fetch for the page (never per-row), so each
                // summary carries its label set for has-tag conditions.
                let pairs: Vec<(String, String)> = objects
                    .iter()
                    .filter_map(|o| {
                        bucket_index
                            .get(&o.bucket_id)
                            .map(|(_, store)| (store.clone(), o.key.clone()))
                    })
                    .collect();
                let mut labels: std::collections::HashMap<(String, String), Vec<String>> =
                    std::collections::HashMap::new();
                for l in self
                    .store
                    .object_labels()
                    .list_for_paths(ws, &pairs)
                    .await
                    .map_err(query_err)?
                {
                    labels.entry((l.store, l.path)).or_default().push(l.label);
                }
                objects
                    .into_iter()
                    .map(|o| object_summary(o, &bucket_index, &labels))
                    .collect()
            }
            other => {
                return Err(Error::invalid(format!(
                    "unknown query_structured operation `{other}` (expected \
                     recent_notes | notes_by_tag | upcoming_events | events_in_range | \
                     open_tasks | tasks_by_status | tasks_by_board | boards | \
                     calendars | recent_objects | objects_by_prefix | unlabeled_objects)"
                )))
            }
        };
        Ok(json!({ "operation": operation, "results": results }))
    }
}

/// Parse a `tasks_by_status` status arg into a [`TaskStatus`], or an `Invalid`
/// error naming the accepted values.
pub(crate) fn parse_task_status(s: &str) -> Result<TaskStatus> {
    match s.trim().to_ascii_lowercase().as_str() {
        "open" => Ok(TaskStatus::Open),
        "in_progress" => Ok(TaskStatus::InProgress),
        "blocked" => Ok(TaskStatus::Blocked),
        "done" => Ok(TaskStatus::Done),
        other => Err(Error::invalid(format!(
            "unknown task status `{other}` (expected open | in_progress | blocked | done)"
        ))),
    }
}

pub(crate) fn query_err(e: catalerum_store::StoreError) -> Error {
    Error::provider(format!("structured query failed: {e}"))
}

/// Map an [`catalerum_ingest::IngestError`] (from the memory dedup seam) onto the
/// tool error type, preserving the store's `NotFound`/`Invalid`/… semantics and
/// treating everything else (vector/embed/graph faults) as a provider error.
pub(crate) fn ingest_err(e: catalerum_ingest::IngestError) -> Error {
    use catalerum_ingest::IngestError as I;
    match e {
        I::Store(s) => s.into(),
        I::Provider(c) => c,
        other => Error::provider(other.to_string()),
    }
}

/// A compact note view for tool results (omits the markdown body to save tokens).
pub(crate) fn note_summary(n: catalerum_core::Note) -> Json {
    json!({
        "id": n.id,
        "title": n.title,
        "tags": n.tags,
        "updated_at": n.updated_at,
    })
}

/// A compact event view for tool results. Includes `labels` (category tags) —
/// cheap and useful for the agent to filter on — but omits the heavier
/// `attachments`/`body`/`attendees`, which `read_event` carries.
pub(crate) fn event_summary(e: catalerum_core::Event) -> Json {
    json!({
        "id": e.id,
        "summary": e.summary,
        "start": e.start,
        "end": e.end,
        "location": e.location,
        "labels": e.labels,
    })
}
