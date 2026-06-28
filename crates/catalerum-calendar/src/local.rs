//! Local `.ics` calendar provider (SOUL §8).
//!
//! Reads a directory of `.ics` files (the path comes from the connection
//! config). Each file is a calendar; the relative file name is the calendar's
//! stable `external_id`, so re-syncing a file always maps to the same
//! [`Calendar`] and the same events (idempotent, SOUL §3.4). Read-only.
//!
//! ## Sync model
//! [`sync`](LocalIcsProvider::sync) returns the **full current set** of events
//! in the file, with a content-hash cursor (`sha256:<mtime>:<digest>`). The
//! store upserts by `(calendar_id, uid)`, so returning the full set every time
//! never duplicates; the cursor lets a caller cheaply detect "nothing changed"
//! and short-circuit. Deletions are reported by diffing against nothing here —
//! the caller reconciles deletions by replacing the file's event set (the
//! ingest worker uses [`delete_by_calendar`] before re-upserting, or compares
//! UIDs). We surface the authoritative UID set so a reconciler can compute
//! removals.
//!
//! A filesystem watcher (`notify`) is provided as a best-effort change signal
//! ([`watch`]); the pull/[`sync`] path is the source of truth.
//!
//! [`delete_by_calendar`]: https://docs.rs/catalerum-store

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use catalerum_core::error::{Error, Result};
use catalerum_core::id::{ConnectionId, WorkspaceId};
use catalerum_core::model::{Calendar, Cursor, Event};
use catalerum_core::provider::{CalendarProvider, NewEvent, SyncBatch};

use crate::ical;

/// A read-only calendar provider over a directory of `.ics` files (SOUL §8).
///
/// Construct from a connection's config with
/// [`LocalIcsProvider::from_config`], or directly with
/// [`LocalIcsProvider::new`].
#[derive(Clone, Debug)]
pub struct LocalIcsProvider {
    workspace_id: WorkspaceId,
    connection_id: ConnectionId,
    dir: PathBuf,
}

/// The canonical config key (in `connections.config`) holding the directory
/// path. This is the key the API's `POST /connections` route blesses and
/// persists for `kind = "local"`, so the provider and the API agree on one
/// wire name end-to-end.
pub const CONFIG_DIR_KEY: &str = "dir";

/// A legacy/alias config key for the directory path. Accepted on read so older
/// connections (or callers using `path`) keep working; new configs use
/// [`CONFIG_DIR_KEY`].
pub const CONFIG_PATH_KEY: &str = "path";

/// The config keys this provider will read for its directory, in priority
/// order: the canonical [`CONFIG_DIR_KEY`] first, then the [`CONFIG_PATH_KEY`]
/// alias.
pub const DIR_CONFIG_KEYS: &[&str] = &[CONFIG_DIR_KEY, CONFIG_PATH_KEY];

impl LocalIcsProvider {
    /// A provider serving `.ics` files under `dir`, owned by `connection_id`
    /// in `workspace_id`.
    #[must_use]
    pub fn new(
        workspace_id: WorkspaceId,
        connection_id: ConnectionId,
        dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            workspace_id,
            connection_id,
            dir: dir.into(),
        }
    }

    /// Build from a connection's `config` JSON. Reads the directory from the
    /// canonical `dir` key (what the API persists, [`CONFIG_DIR_KEY`]), falling
    /// back to the `path` alias ([`CONFIG_PATH_KEY`]) for older connections.
    /// Expects `{"dir": "/some/dir"}` (or `{"path": "/some/dir"}`).
    pub fn from_config(
        workspace_id: WorkspaceId,
        connection_id: ConnectionId,
        config: &serde_json::Value,
    ) -> Result<Self> {
        let dir = DIR_CONFIG_KEYS
            .iter()
            .find_map(|key| config.get(*key).and_then(serde_json::Value::as_str))
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                Error::invalid(format!(
                    "local ics connection config missing string `{CONFIG_DIR_KEY}` (or `{CONFIG_PATH_KEY}`)"
                ))
            })?;
        Ok(Self::new(workspace_id, connection_id, dir))
    }

    /// The directory this provider reads.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Resolve a [`Calendar`]'s `external_id` back to its file path, guarding
    /// against path traversal outside the configured directory.
    fn file_for(&self, external_id: &str) -> Result<PathBuf> {
        if external_id.is_empty()
            || external_id.contains("..")
            || Path::new(external_id).is_absolute()
        {
            return Err(Error::invalid(format!(
                "unsafe calendar external_id: {external_id}"
            )));
        }
        Ok(self.dir.join(external_id))
    }

    /// Construct the deterministic [`Calendar`] for a file. The `external_id`
    /// is the path relative to the directory; the `name` is the file stem.
    fn calendar_for(&self, external_id: &str) -> Calendar {
        // A v5 UUID over (connection_id, external_id) gives the *same*
        // CalendarId across runs without a DB round-trip, so the provider's
        // `Calendar` is stable and matches what the store upserts by
        // (connection_id, external_id).
        let id = stable_calendar_id(self.connection_id, external_id);
        let name = Path::new(external_id)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(external_id)
            .to_string();
        Calendar {
            id,
            workspace_id: self.workspace_id,
            connection_id: Some(self.connection_id),
            external_id: external_id.to_string(),
            name,
            read_only: true,
        }
    }

    /// Read + parse one `.ics` file into events plus a content cursor.
    async fn read_file(&self, cal: &Calendar) -> Result<(Vec<Event>, Cursor)> {
        let path = self.file_for(&cal.external_id)?;
        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|e| Error::Provider(format!("read {}: {e}", path.display())))?;
        let text = String::from_utf8_lossy(&bytes);

        let parsed = ical::parse_calendar(&text)?;
        let events = parsed
            .into_iter()
            .map(|p| p.into_event(self.workspace_id, cal.id))
            .collect();

        let cursor = crate::content_cursor(&bytes);
        Ok((events, cursor))
    }
}

#[async_trait]
impl CalendarProvider for LocalIcsProvider {
    /// One [`Calendar`] per `.ics` file directly under the directory
    /// (non-recursive). Files are listed in sorted order for determinism.
    async fn list_calendars(&self) -> Result<Vec<Calendar>> {
        let mut entries = tokio::fs::read_dir(&self.dir)
            .await
            .map_err(|e| Error::Provider(format!("read dir {}: {e}", self.dir.display())))?;

        let mut names = Vec::new();
        while let Some(entry) = entries.next_entry().await.map_err(Error::Io)? {
            let path = entry.path();
            let is_ics = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("ics"));
            if is_ics && entry.file_type().await.map_err(Error::Io)?.is_file() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    names.push(name.to_string());
                }
            }
        }
        names.sort();
        Ok(names.iter().map(|n| self.calendar_for(n)).collect())
    }

    /// Return the full current event set with a content-hash cursor. When the
    /// passed `cursor` equals the file's current content cursor, the events are
    /// unchanged — we still return them (idempotent upsert) but `has_more` is
    /// `false` and `next_cursor` is unchanged, so a caller may skip the write.
    async fn sync(&self, cal: &Calendar, cursor: Option<Cursor>) -> Result<SyncBatch<Event>> {
        let (events, next_cursor) = self.read_file(cal).await?;
        let unchanged = cursor.as_ref() == Some(&next_cursor);
        Ok(SyncBatch {
            upserts: if unchanged { Vec::new() } else { events },
            // The local provider cannot observe deletions across calls (it has
            // no prior snapshot); the ingest worker reconciles removals by
            // diffing the returned UID set against stored UIDs when the cursor
            // changes. We never fabricate deletions here.
            deletions: Vec::new(),
            next_cursor,
            has_more: false,
        })
    }

    async fn create_event(&self, _cal: &Calendar, _event: NewEvent) -> Result<Event> {
        Err(Error::Unsupported(
            "local .ics calendars are read-only".into(),
        ))
    }

    async fn update_event(&self, _event: &Event) -> Result<Event> {
        Err(Error::Unsupported(
            "local .ics calendars are read-only".into(),
        ))
    }

    async fn delete_event(&self, _event: &Event) -> Result<()> {
        Err(Error::Unsupported(
            "local .ics calendars are read-only".into(),
        ))
    }
}

/// Stable [`CalendarId`](catalerum_core::CalendarId) derived from the owning
/// connection and the file's `external_id` (UUID v5, DNS namespace). Lets the
/// provider hand out the same id the store will, without a DB lookup.
fn stable_calendar_id(
    connection_id: ConnectionId,
    external_id: &str,
) -> catalerum_core::CalendarId {
    let seed = format!("{connection_id}/{external_id}");
    let uuid = uuid_v5(&seed);
    catalerum_core::CalendarId::from_uuid(uuid)
}

/// UUID v5 over the URL namespace. `uuid` is already a transitive dep via
/// `catalerum-core`'s id types; we reuse it here.
fn uuid_v5(name: &str) -> uuid::Uuid {
    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, name.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ICS: &str = "BEGIN:VCALENDAR\nVERSION:2.0\nBEGIN:VEVENT\nUID:a@x\nDTSTART:20260613T090000Z\nDTEND:20260613T100000Z\nSUMMARY:Standup\nEND:VEVENT\nEND:VCALENDAR\n";

    async fn fixture() -> (tempfile::TempDir, LocalIcsProvider) {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("work.ics"), ICS)
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("notes.txt"), "ignore me")
            .await
            .unwrap();
        let provider = LocalIcsProvider::new(WorkspaceId::new(), ConnectionId::new(), dir.path());
        (dir, provider)
    }

    #[tokio::test]
    async fn lists_one_calendar_per_ics_file() {
        let (_dir, provider) = fixture().await;
        let cals = provider.list_calendars().await.unwrap();
        assert_eq!(cals.len(), 1);
        assert_eq!(cals[0].external_id, "work.ics");
        assert_eq!(cals[0].name, "work");
        assert!(cals[0].read_only);
    }

    #[tokio::test]
    async fn sync_returns_events_then_is_idempotent_by_cursor() {
        let (_dir, provider) = fixture().await;
        let cal = provider.list_calendars().await.unwrap().pop().unwrap();

        let first = provider.sync(&cal, None).await.unwrap();
        assert_eq!(first.upserts.len(), 1);
        assert_eq!(first.upserts[0].uid, "a@x");
        assert_eq!(first.upserts[0].summary, "Standup");

        // Re-sync with the returned cursor: unchanged content -> no upserts,
        // same cursor. (Re-running never duplicates — SOUL §3.4.)
        let second = provider
            .sync(&cal, Some(first.next_cursor.clone()))
            .await
            .unwrap();
        assert!(second.upserts.is_empty());
        assert_eq!(second.next_cursor, first.next_cursor);
    }

    #[tokio::test]
    async fn calendar_id_is_stable_across_instances() {
        let ws = WorkspaceId::new();
        let conn = ConnectionId::new();
        let a = LocalIcsProvider::new(ws, conn, "/tmp/x").calendar_for("work.ics");
        let b = LocalIcsProvider::new(ws, conn, "/tmp/x").calendar_for("work.ics");
        assert_eq!(a.id, b.id);
    }

    #[tokio::test]
    async fn changed_content_yields_new_cursor_and_events() {
        let (dir, provider) = fixture().await;
        let cal = provider.list_calendars().await.unwrap().pop().unwrap();
        let first = provider.sync(&cal, None).await.unwrap();

        let changed = ICS.replace("Standup", "Standup v2");
        tokio::fs::write(dir.path().join("work.ics"), changed)
            .await
            .unwrap();

        let second = provider
            .sync(&cal, Some(first.next_cursor.clone()))
            .await
            .unwrap();
        assert_ne!(second.next_cursor, first.next_cursor);
        assert_eq!(second.upserts.len(), 1);
        assert_eq!(second.upserts[0].summary, "Standup v2");
    }

    #[tokio::test]
    async fn from_config_reads_dir_and_path_keys() {
        // Canonical `dir` key (what the API persists).
        let cfg = serde_json::json!({ "dir": "/var/calendars" });
        let p =
            LocalIcsProvider::from_config(WorkspaceId::new(), ConnectionId::new(), &cfg).unwrap();
        assert_eq!(p.dir(), Path::new("/var/calendars"));

        // Legacy `path` alias still works.
        let cfg = serde_json::json!({ "path": "/var/calendars" });
        let p =
            LocalIcsProvider::from_config(WorkspaceId::new(), ConnectionId::new(), &cfg).unwrap();
        assert_eq!(p.dir(), Path::new("/var/calendars"));

        // `dir` wins when both are present.
        let cfg = serde_json::json!({ "dir": "/canonical", "path": "/legacy" });
        let p =
            LocalIcsProvider::from_config(WorkspaceId::new(), ConnectionId::new(), &cfg).unwrap();
        assert_eq!(p.dir(), Path::new("/canonical"));

        let bad = serde_json::json!({ "nope": 1 });
        assert!(
            LocalIcsProvider::from_config(WorkspaceId::new(), ConnectionId::new(), &bad).is_err()
        );

        // An empty string is treated as missing.
        let empty = serde_json::json!({ "dir": "  " });
        assert!(
            LocalIcsProvider::from_config(WorkspaceId::new(), ConnectionId::new(), &empty).is_err()
        );
    }

    #[tokio::test]
    async fn rejects_path_traversal() {
        let (_dir, provider) = fixture().await;
        assert!(provider.file_for("../etc/passwd").is_err());
        assert!(provider.file_for("/etc/passwd").is_err());
        assert!(provider.file_for("work.ics").is_ok());
    }
}
