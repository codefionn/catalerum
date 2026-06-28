//! Store-backed MCP providers (SOUL §26): a workspace's **skills → prompts** and
//! its **notes / Kanban tasks / calendar / files → resources** (read-view context).
//!
//! These power both MCP transports — the `catalerum mcp` stdio server (the binary)
//! and the authenticated `POST /mcp` HTTP route ([`crate::routes::mcp`]) — so the
//! two stay at feature parity from one definition. Each is scoped to a single
//! workspace; the HTTP route builds one per request from the bearer's workspace,
//! the stdio server one at startup. Every query failure is logged and degrades to
//! empty / an "unavailable" body — a provider never breaks the MCP session.

use std::collections::HashMap;

use catalerum_core::{ColumnId, Task, WorkspaceId};
use catalerum_mcp::{
    PromptContent, PromptInfo, PromptProvider, ResourceContent, ResourceInfo, ResourceProvider,
};
use catalerum_store::{
    DateRange, Store, DEFAULT_EVENT_LIMIT, DEFAULT_NOTE_LIMIT, DEFAULT_OBJECT_LIMIT,
};

/// Stable URIs for the MCP read-view resources.
const RESOURCE_NOTES: &str = "catalerum://notes";
const RESOURCE_TASKS: &str = "catalerum://tasks";
const RESOURCE_CALENDAR: &str = "catalerum://calendar";
const RESOURCE_FILES: &str = "catalerum://files";

/// Exposes a workspace's skills (SOUL §23) as MCP prompts (§26): each skill is a
/// prompt whose `prompts/get` returns its markdown runbook.
pub struct SkillPromptProvider {
    store: Store,
    workspace_id: WorkspaceId,
}

impl SkillPromptProvider {
    /// A prompt provider over `workspace_id`'s skills.
    #[must_use]
    pub fn new(store: Store, workspace_id: WorkspaceId) -> Self {
        Self {
            store,
            workspace_id,
        }
    }
}

#[async_trait::async_trait]
impl PromptProvider for SkillPromptProvider {
    async fn list(&self) -> Vec<PromptInfo> {
        match self
            .store
            .skills()
            .list_by_workspace(self.workspace_id)
            .await
        {
            Ok(skills) => skills
                .into_iter()
                .map(|s| PromptInfo {
                    name: s.name,
                    description: s.description,
                })
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "failed to list skills for MCP prompts");
                Vec::new()
            }
        }
    }

    async fn get(&self, name: &str) -> Option<PromptContent> {
        match self
            .store
            .skills()
            .get_by_name(self.workspace_id, name)
            .await
        {
            Ok(Some(skill)) => Some(PromptContent {
                description: Some(skill.description),
                text: skill.instructions_md,
            }),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!(error = %e, skill = %name, "failed to load skill for MCP prompt");
                None
            }
        }
    }
}

/// Exposes a workspace's notes, Kanban tasks, calendar, and files as MCP resources
/// (SOUL §26): read views (markdown) an external agent can attach as context.
pub struct WorkspaceResourceProvider {
    store: Store,
    workspace_id: WorkspaceId,
}

impl WorkspaceResourceProvider {
    /// A resource provider over `workspace_id`'s notes / tasks / calendar / files.
    #[must_use]
    pub fn new(store: Store, workspace_id: WorkspaceId) -> Self {
        Self {
            store,
            workspace_id,
        }
    }
}

/// The read-view resources a [`WorkspaceResourceProvider`] advertises (SOUL §26),
/// as a free fn so the catalogue contract is unit-testable without a store/DB.
fn resource_infos() -> Vec<ResourceInfo> {
    vec![
        ResourceInfo {
            uri: RESOURCE_NOTES.into(),
            name: "Notes".into(),
            description: "The workspace's notes (title + tags).".into(),
            mime_type: "text/markdown".into(),
        },
        ResourceInfo {
            uri: RESOURCE_TASKS.into(),
            name: "Tasks".into(),
            description: "The workspace's Kanban boards and their tasks, by column.".into(),
            mime_type: "text/markdown".into(),
        },
        ResourceInfo {
            uri: RESOURCE_CALENDAR.into(),
            name: "Calendar".into(),
            description:
                "The workspace's upcoming calendar events (summary, time, location, labels).".into(),
            mime_type: "text/markdown".into(),
        },
        ResourceInfo {
            uri: RESOURCE_FILES.into(),
            name: "Files".into(),
            description: "The workspace's stored files (key + content type).".into(),
            mime_type: "text/markdown".into(),
        },
    ]
}

#[async_trait::async_trait]
impl ResourceProvider for WorkspaceResourceProvider {
    async fn list(&self) -> Vec<ResourceInfo> {
        resource_infos()
    }

    async fn read(&self, uri: &str) -> Option<ResourceContent> {
        let text = match uri {
            RESOURCE_NOTES => self.render_notes().await,
            RESOURCE_TASKS => self.render_tasks().await,
            RESOURCE_CALENDAR => self.render_calendar().await,
            RESOURCE_FILES => self.render_files().await,
            _ => return None,
        };
        Some(ResourceContent {
            uri: uri.into(),
            mime_type: "text/markdown".into(),
            text,
        })
    }
}

impl WorkspaceResourceProvider {
    async fn render_notes(&self) -> String {
        match self
            .store
            .notes()
            .list_by_workspace(self.workspace_id, DEFAULT_NOTE_LIMIT)
            .await
        {
            Ok(notes) if !notes.is_empty() => {
                let mut out = String::from("# Notes\n\n");
                for n in notes {
                    let tags = if n.tags.is_empty() {
                        String::new()
                    } else {
                        format!(" _({})_", n.tags.join(", "))
                    };
                    out.push_str(&format!("- **{}**{} — `{}`\n", n.title, tags, n.id));
                }
                out
            }
            Ok(_) => "# Notes\n\n_No notes yet._\n".to_string(),
            Err(e) => {
                tracing::warn!(error = %e, "MCP notes resource read failed");
                "# Notes\n\n_Unavailable._\n".to_string()
            }
        }
    }

    async fn render_tasks(&self) -> String {
        let boards = match self
            .store
            .boards()
            .list_by_workspace(self.workspace_id)
            .await
        {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "MCP tasks resource read failed");
                return "# Tasks\n\n_Unavailable._\n".to_string();
            }
        };
        if boards.is_empty() {
            return "# Tasks\n\n_No boards yet._\n".to_string();
        }
        // Fetch every task once and group by column, rather than a query per column
        // (was 1 + boards×columns; now 2). `list_by_workspace` is already ordered
        // board → column-ordinal → task-ordinal, so grouping by `column_id` keeps each
        // column's tasks in board order. A fetch failure degrades the whole resource,
        // exactly like the boards fetch above.
        let tasks_by_col: HashMap<ColumnId, Vec<Task>> = match self
            .store
            .tasks()
            .list_by_workspace(self.workspace_id)
            .await
        {
            Ok(tasks) => {
                let mut map: HashMap<ColumnId, Vec<Task>> = HashMap::new();
                for t in tasks {
                    map.entry(t.column_id).or_default().push(t);
                }
                map
            }
            Err(e) => {
                tracing::warn!(error = %e, "MCP tasks resource read failed");
                return "# Tasks\n\n_Unavailable._\n".to_string();
            }
        };
        let mut out = String::from("# Tasks\n\n");
        for board in boards {
            out.push_str(&format!("## {}\n\n", board.name));
            for col in &board.columns {
                out.push_str(&format!("### {}\n", col.name));
                match tasks_by_col.get(&col.id) {
                    Some(tasks) if !tasks.is_empty() => {
                        for t in tasks {
                            out.push_str(&format!("- {} — `{}`\n", t.title, t.id));
                        }
                    }
                    _ => out.push_str("_(empty)_\n"),
                }
                out.push('\n');
            }
        }
        out
    }

    async fn render_calendar(&self) -> String {
        // **Upcoming** events: `list_by_workspace` orders `starts_at ASC` and applies
        // the limit, so `from = now` yields the soonest `DEFAULT_EVENT_LIMIT` events —
        // not the oldest (which `from: None` would, burying the live schedule on a
        // busy calendar). Mirrors `query_structured`'s `upcoming_events` (tools.rs).
        match self
            .store
            .events()
            .list_by_workspace(
                self.workspace_id,
                None,
                DateRange {
                    from: Some(chrono::Utc::now()),
                    to: None,
                },
                DEFAULT_EVENT_LIMIT,
            )
            .await
        {
            Ok(events) if !events.is_empty() => {
                let mut out = String::from("# Calendar\n\n");
                for e in events {
                    let loc = match &e.location {
                        Some(l) if !l.is_empty() => format!(" @ {l}"),
                        _ => String::new(),
                    };
                    // Surface labels like the notes read-view surfaces tags, so an
                    // external agent can see "what's on my calendar near topic X".
                    let labels = if e.labels.is_empty() {
                        String::new()
                    } else {
                        format!(" _({})_", e.labels.join(", "))
                    };
                    out.push_str(&format!(
                        "- **{}** {} – {}{}{} — `{}`\n",
                        e.summary,
                        e.start.to_rfc3339(),
                        e.end.to_rfc3339(),
                        loc,
                        labels,
                        e.id
                    ));
                }
                out
            }
            Ok(_) => "# Calendar\n\n_No events yet._\n".to_string(),
            Err(e) => {
                tracing::warn!(error = %e, "MCP calendar resource read failed");
                "# Calendar\n\n_Unavailable._\n".to_string()
            }
        }
    }

    async fn render_files(&self) -> String {
        match self
            .store
            .objects()
            .list_by_workspace(self.workspace_id, "", DEFAULT_OBJECT_LIMIT)
            .await
        {
            Ok(objects) if !objects.is_empty() => {
                let mut out = String::from("# Files\n\n");
                for o in objects {
                    let ct = match &o.content_type {
                        Some(c) if !c.is_empty() => format!(" _({c})_"),
                        _ => String::new(),
                    };
                    out.push_str(&format!("- **{}**{} — `{}`\n", o.key, ct, o.id));
                }
                out
            }
            Ok(_) => "# Files\n\n_No files yet._\n".to_string(),
            Err(e) => {
                tracing::warn!(error = %e, "MCP files resource read failed");
                "# Files\n\n_Unavailable._\n".to_string()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_all_four_read_view_resources() {
        // SOUL §26 names notes, tasks, calendar, files as read-view resources.
        let infos = resource_infos();
        let uris: Vec<&str> = infos.iter().map(|r| r.uri.as_str()).collect();
        assert_eq!(
            uris,
            vec![
                RESOURCE_NOTES,
                RESOURCE_TASKS,
                RESOURCE_CALENDAR,
                RESOURCE_FILES
            ],
        );
        assert!(infos.iter().all(|r| r.mime_type == "text/markdown"));
        assert!(infos
            .iter()
            .all(|r| !r.name.is_empty() && !r.description.is_empty()));
    }

    fn db_url() -> Option<String> {
        std::env::var("CATALERUM_TEST_DATABASE_URL")
            .or_else(|_| std::env::var("DATABASE_URL"))
            .ok()
    }

    #[tokio::test]
    async fn renders_calendar_files_and_groups_tasks_without_n_plus_one() {
        let Some(url) = db_url() else {
            eprintln!("skipping MCP resource render test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
            return;
        };
        use catalerum_core::model::ConnectionKind;
        use catalerum_store::{UpsertEvent, UpsertObject};

        let store = Store::connect(&url).await.expect("store");
        let ws = store
            .workspaces()
            .create("mcpres", &format!("mcpres-{}", uuid::Uuid::new_v4()))
            .await
            .expect("ws");

        // Tasks: a board with the default columns; a task in the first column only, so
        // a later column exercises the empty-column path.
        let board = store
            .boards()
            .create(ws.id, "Sprint", &[])
            .await
            .expect("board");
        let first_col = board.columns.first().expect("a default column").clone();
        let later_col = board
            .columns
            .get(1)
            .expect("a second default column")
            .clone();
        store
            .tasks()
            .create(
                ws.id,
                board.id,
                first_col.id,
                "Ship MCP resources",
                "",
                None,
            )
            .await
            .expect("task");

        // Calendar: a connection + calendar + one event with a location.
        let cal_conn = store
            .connections()
            .ensure(ws.id, ConnectionKind::Calendar, "cal", None, None)
            .await
            .expect("cal conn");
        let cal = store
            .calendars()
            .upsert(ws.id, cal_conn.id, "ext-cal", "Work", false)
            .await
            .expect("cal");
        let now = chrono::Utc::now();
        // A clearly-upcoming event (must appear) and a past one (must be excluded —
        // the read-view shows the upcoming schedule, not ancient history).
        store
            .events()
            .upsert_by_uid(&UpsertEvent {
                workspace_id: ws.id,
                calendar_id: cal.id,
                uid: "evt-future",
                starts_at: now + chrono::Duration::hours(2),
                ends_at: now + chrono::Duration::hours(3),
                all_day: false,
                rrule: None,
                summary: "Quarterly review",
                location: Some("Room 5"),
                body: None,
                attendees: &[],
                labels: &["planning".to_string()],
                attachments: &[],
                etag: None,
                sequence: 0,
            })
            .await
            .expect("future event");
        store
            .events()
            .upsert_by_uid(&UpsertEvent {
                workspace_id: ws.id,
                calendar_id: cal.id,
                uid: "evt-past",
                starts_at: now - chrono::Duration::days(3),
                ends_at: now - chrono::Duration::days(3) + chrono::Duration::hours(1),
                all_day: false,
                rrule: None,
                summary: "Ancient standup",
                location: None,
                body: None,
                attendees: &[],
                labels: &[],
                attachments: &[],
                etag: None,
                sequence: 0,
            })
            .await
            .expect("past event");

        // Files: a storage connection + bucket + one object.
        let st_conn = store
            .connections()
            .ensure(ws.id, ConnectionKind::Storage, "storage", None, None)
            .await
            .expect("st conn");
        let bucket = store
            .buckets()
            .ensure(ws.id, st_conn.id, "files", None)
            .await
            .expect("bucket");
        store
            .objects()
            .upsert(&UpsertObject {
                workspace_id: ws.id,
                bucket_id: bucket.id,
                key: "docs/spec.pdf",
                size: 10,
                content_type: Some("application/pdf"),
                etag: None,
                last_modified: now,
                sha256: None,
            })
            .await
            .expect("object");

        let provider = WorkspaceResourceProvider::new(store.clone(), ws.id);

        // Calendar resource renders the UPCOMING event's summary + location, and
        // excludes the past one (proving `from: now`, not the oldest-2000 slice).
        let cal_md = provider
            .read(RESOURCE_CALENDAR)
            .await
            .expect("calendar resource")
            .text;
        assert!(cal_md.contains("Quarterly review"), "calendar: {cal_md}");
        assert!(cal_md.contains("Room 5"));
        // Labels surface in the read-view (the topic-near-calendar use case).
        assert!(cal_md.contains("_(planning)_"), "calendar labels: {cal_md}");
        assert!(
            !cal_md.contains("Ancient standup"),
            "a past event is not in the upcoming read-view: {cal_md}"
        );

        // Files resource renders the key + content type.
        let files_md = provider
            .read(RESOURCE_FILES)
            .await
            .expect("files resource")
            .text;
        assert!(files_md.contains("docs/spec.pdf"), "files: {files_md}");
        assert!(files_md.contains("application/pdf"));

        // Tasks resource groups the task under its column and still marks the empty
        // column `_(empty)_` — proving the single-fetch grouping (N+1 fix) preserves
        // behaviour.
        let tasks_md = provider
            .read(RESOURCE_TASKS)
            .await
            .expect("tasks resource")
            .text;
        assert!(tasks_md.contains(&format!("### {}", first_col.name)));
        assert!(tasks_md.contains("Ship MCP resources"), "tasks: {tasks_md}");
        assert!(tasks_md.contains(&format!("### {}", later_col.name)));
        assert!(
            tasks_md.contains("_(empty)_"),
            "an empty column still renders _(empty)_: {tasks_md}"
        );

        // An unknown resource uri → None (contract unchanged).
        assert!(provider.read("catalerum://nope").await.is_none());
    }
}
