use catalerum_core::id::{LinkId, NoteId, ObjectLabelId, WorkspaceId};
use catalerum_core::model::{Author, Link, Note, ObjectLabel, SourceRef};
use sqlx::types::Json;
use crate::error::{Result, StoreError};
use crate::rows::{author_to_parts, LinkRow, NoteRow, ObjectLabelRow, source_to_parts};
use super::{map, PgPool};

// ===========================================================================
// Notes
// ===========================================================================

/// The full set of `notes` columns, in row order (for `SELECT`/`RETURNING`).
const NOTE_COLS: &str = "id, workspace_id, author_kind, author_id, title, markdown, tags, \
     created_at, updated_at";

/// Default cap on [`NoteRepo::list_by_workspace`] (SOUL §18 — every read is
/// bounded, so a workspace with thousands of notes can't balloon the agent context
/// or the API payload). The list is bounded to this many (most-recently-edited
/// first); normal note collections fall well under it and are unaffected. Mirrors
/// [`DEFAULT_OBJECT_LIMIT`](crate::repo::DEFAULT_OBJECT_LIMIT).
pub const DEFAULT_NOTE_LIMIT: i64 = 1000;

/// CRUD for the `notes` table — user- or LLM-authored markdown notes (SOUL §21).
/// Every query is workspace-filtered (SOUL §6.1/§18); notes list
/// most-recently-edited first.
#[derive(Clone, Debug)]
pub struct NoteRepo {
    pool: PgPool,
}

impl NoteRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create a note in a workspace, authored by `author` (a user or an agent,
    /// SOUL §21). Returns the stored note.
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        author: Author,
        title: &str,
        markdown: &str,
        tags: &[String],
    ) -> Result<Note> {
        let id = NoteId::new().into_uuid();
        let (author_kind, author_id) = author_to_parts(author);
        let row: NoteRow = sqlx::query_as(assert_sql_safe!(
            "INSERT INTO notes
                 (id, workspace_id, author_kind, author_id, title, markdown, tags)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             RETURNING {NOTE_COLS}"
        ))
        .bind(id)
        .bind(workspace_id.into_uuid())
        .bind(author_kind)
        .bind(author_id)
        .bind(title)
        .bind(markdown)
        .bind(Json(tags.to_vec()))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Fetch a note, scoped to its workspace.
    pub async fn get(&self, workspace_id: WorkspaceId, id: NoteId) -> Result<Note> {
        let row: NoteRow = sqlx::query_as(assert_sql_safe!(
            "SELECT {NOTE_COLS} FROM notes
             WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// List a workspace's notes, most-recently-edited first, bounded to `limit`
    /// rows (floored at 1, see [`DEFAULT_NOTE_LIMIT`]) so a large note collection
    /// can't return an unbounded set (SOUL §18). The bound applies after the
    /// most-recent ordering, so you get the newest `limit` notes.
    pub async fn list_by_workspace(
        &self,
        workspace_id: WorkspaceId,
        limit: i64,
    ) -> Result<Vec<Note>> {
        let rows: Vec<NoteRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {NOTE_COLS} FROM notes
             WHERE workspace_id = $1
             ORDER BY updated_at DESC, id ASC
             LIMIT $2"
        ))
        .bind(workspace_id.into_uuid())
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(Note::try_from).collect()
    }

    /// Update a note's `title`, `markdown`, and `tags` (workspace-scoped),
    /// bumping `updated_at`. The author is immutable. Returns the updated note,
    /// or [`StoreError::NotFound`] if no such note exists in the workspace.
    pub async fn update(
        &self,
        workspace_id: WorkspaceId,
        id: NoteId,
        title: &str,
        markdown: &str,
        tags: &[String],
    ) -> Result<Note> {
        let row: NoteRow = sqlx::query_as(assert_sql_safe!(
            "UPDATE notes SET title = $3, markdown = $4, tags = $5, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2
             RETURNING {NOTE_COLS}"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(title)
        .bind(markdown)
        .bind(Json(tags.to_vec()))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Delete a note, workspace-scoped. [`StoreError::NotFound`] if absent.
    pub async fn delete(&self, workspace_id: WorkspaceId, id: NoteId) -> Result<()> {
        let res = sqlx::query("DELETE FROM notes WHERE id = $1 AND workspace_id = $2")
            .bind(id.into_uuid())
            .bind(workspace_id.into_uuid())
            .execute(&self.pool)
            .await
            .map_err(map)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }
}

// ===========================================================================
// Links — relationships between objects (SOUL §5/§6.3)
// ===========================================================================

const LINK_COLS: &str = "id, workspace_id, from_kind, from_id, to_kind, to_id, label, note, \
     author_kind, author_id, created_at, updated_at";

/// Default cap on [`LinkRepo`] list reads (SOUL §18 — every read is bounded).
/// Mirrors [`DEFAULT_NOTE_LIMIT`].
pub const DEFAULT_LINK_LIMIT: i64 = 1000;

/// CRUD for the `links` table — user/agent-authored directed relationships
/// between two objects (SOUL §5/§6.3). Every query is workspace-filtered
/// (SOUL §6.1/§18). Both endpoints are the core [`SourceRef`], stored split
/// across `(from_kind, from_id)` / `(to_kind, to_id)` via [`source_to_parts`].
#[derive(Clone, Debug)]
pub struct LinkRepo {
    pool: PgPool,
}

impl LinkRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create a directed `from → to` link in a workspace, authored by `author`.
    /// **Idempotent** on `(workspace_id, from, to, label)` (a NULL label folds to
    /// `''`): re-creating the same relationship refreshes its `note`/`updated_at`
    /// and returns the *existing* row (id preserved), never a duplicate. Rejects a
    /// self-link (`from == to`) with [`StoreError::Invalid`].
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        author: Author,
        from: &SourceRef,
        to: &SourceRef,
        label: Option<&str>,
        note: Option<&str>,
    ) -> Result<Link> {
        if from == to {
            return Err(StoreError::invalid("a link cannot point at itself"));
        }
        let id = LinkId::new().into_uuid();
        let (from_kind, from_id) = source_to_parts(from);
        let (to_kind, to_id) = source_to_parts(to);
        let (author_kind, author_id) = author_to_parts(author);
        let row: LinkRow = sqlx::query_as(assert_sql_safe!(
            "INSERT INTO links
                 (id, workspace_id, from_kind, from_id, to_kind, to_id, label, note,
                  author_kind, author_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
             ON CONFLICT (workspace_id, from_kind, from_id, to_kind, to_id, COALESCE(label, ''))
                 DO UPDATE SET note = EXCLUDED.note, updated_at = CURRENT_TIMESTAMP
             RETURNING {LINK_COLS}"
        ))
        .bind(id)
        .bind(workspace_id.into_uuid())
        .bind(from_kind)
        .bind(from_id)
        .bind(to_kind)
        .bind(to_id)
        .bind(label)
        .bind(note)
        .bind(author_kind)
        .bind(author_id)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Fetch a link, scoped to its workspace.
    pub async fn get(&self, workspace_id: WorkspaceId, id: LinkId) -> Result<Link> {
        let row: LinkRow = sqlx::query_as(assert_sql_safe!(
            "SELECT {LINK_COLS} FROM links
             WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// List a workspace's links, most-recently-touched first, bounded to `limit`
    /// rows (floored at 1, see [`DEFAULT_LINK_LIMIT`]).
    pub async fn list_by_workspace(
        &self,
        workspace_id: WorkspaceId,
        limit: i64,
    ) -> Result<Vec<Link>> {
        let rows: Vec<LinkRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {LINK_COLS} FROM links
             WHERE workspace_id = $1
             ORDER BY updated_at DESC, id ASC
             LIMIT $2"
        ))
        .bind(workspace_id.into_uuid())
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(Link::try_from).collect()
    }

    /// Every link touching `endpoint` in **either** direction (as `from` *or* as
    /// `to`) — "what is related to X" — workspace-scoped, most-recently-touched
    /// first, bounded to `limit` (floored at 1). Served by `links_from_idx` /
    /// `links_to_idx`.
    pub async fn list_for(
        &self,
        workspace_id: WorkspaceId,
        endpoint: &SourceRef,
        limit: i64,
    ) -> Result<Vec<Link>> {
        let (kind, id) = source_to_parts(endpoint);
        let rows: Vec<LinkRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {LINK_COLS} FROM links
             WHERE workspace_id = $1
               AND ((from_kind = $2 AND from_id = $3) OR (to_kind = $2 AND to_id = $3))
             ORDER BY updated_at DESC, id ASC
             LIMIT $4"
        ))
        .bind(workspace_id.into_uuid())
        .bind(kind)
        .bind(id)
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(Link::try_from).collect()
    }

    /// Delete a link, workspace-scoped. [`StoreError::NotFound`] if absent.
    pub async fn delete(&self, workspace_id: WorkspaceId, id: LinkId) -> Result<()> {
        let res = sqlx::query("DELETE FROM links WHERE id = $1 AND workspace_id = $2")
            .bind(id.into_uuid())
            .bind(workspace_id.into_uuid())
            .execute(&self.pool)
            .await
            .map_err(map)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }
}

// ===========================================================================
// Object labels — user/agent tags on stored files & directories (SOUL §9)
// ===========================================================================

/// The default cap on a labels listing so a large label set can't return an
/// unbounded payload (mirrors [`DEFAULT_LINK_LIMIT`]).
pub const DEFAULT_LABEL_LIMIT: i64 = 1000;

const OBJECT_LABEL_COLS: &str =
    "id, workspace_id, store, path, is_dir, label, author_kind, author_id, created_at";

/// CRUD for the `object_labels` table — free-text labels on a store's files and
/// directories (SOUL §9). A label is keyed by `(store, path)`, so it can tag a
/// **directory** (no object row) or an uncatalogued file. Every query is
/// workspace-filtered (SOUL §18).
#[derive(Clone, Debug)]
pub struct ObjectLabelRepo {
    pool: PgPool,
}

impl ObjectLabelRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Apply `label` to `path` in `store`, authored by `author`. **Idempotent** on
    /// `(workspace_id, store, path, label)`: re-applying the same label refreshes
    /// `is_dir`/`created_at` and returns the *existing* row (id preserved), never a
    /// duplicate. Rejects a blank `label` or `path` with [`StoreError::Invalid`].
    pub async fn add(
        &self,
        workspace_id: WorkspaceId,
        author: Author,
        store: &str,
        path: &str,
        is_dir: bool,
        label: &str,
    ) -> Result<ObjectLabel> {
        let path = path.trim();
        let label = label.trim();
        if path.is_empty() {
            return Err(StoreError::invalid("a label's path must not be empty"));
        }
        if label.is_empty() {
            return Err(StoreError::invalid("a label must not be empty"));
        }
        let id = ObjectLabelId::new().into_uuid();
        let (author_kind, author_id) = author_to_parts(author);
        let row: ObjectLabelRow = sqlx::query_as(assert_sql_safe!(
            "INSERT INTO object_labels
                 (id, workspace_id, store, path, is_dir, label, author_kind, author_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (workspace_id, store, path, label)
                 DO UPDATE SET is_dir = EXCLUDED.is_dir
             RETURNING {OBJECT_LABEL_COLS}"
        ))
        .bind(id)
        .bind(workspace_id.into_uuid())
        .bind(store)
        .bind(path)
        .bind(is_dir)
        .bind(label)
        .bind(author_kind)
        .bind(author_id)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Every label on one `(store, path)`, oldest-first, workspace-scoped.
    pub async fn list_for(
        &self,
        workspace_id: WorkspaceId,
        store: &str,
        path: &str,
    ) -> Result<Vec<ObjectLabel>> {
        let rows: Vec<ObjectLabelRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {OBJECT_LABEL_COLS} FROM object_labels
             WHERE workspace_id = $1 AND store = $2 AND path = $3
             ORDER BY created_at ASC, id ASC"
        ))
        .bind(workspace_id.into_uuid())
        .bind(store)
        .bind(path)
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(ObjectLabel::try_from).collect()
    }

    /// A store's labels, optionally restricted to paths under `prefix` (a *literal*
    /// prefix — empty = the whole store), most-recent first, bounded to `limit`
    /// (floored at 1). The Files panel badges its tree from this.
    pub async fn list_by_store(
        &self,
        workspace_id: WorkspaceId,
        store: &str,
        prefix: &str,
        limit: i64,
    ) -> Result<Vec<ObjectLabel>> {
        let rows: Vec<ObjectLabelRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {OBJECT_LABEL_COLS} FROM object_labels
             WHERE workspace_id = $1 AND store = $2
               AND ($3 = '' OR starts_with(path, $3))
             ORDER BY created_at DESC, id ASC
             LIMIT $4"
        ))
        .bind(workspace_id.into_uuid())
        .bind(store)
        .bind(prefix)
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(ObjectLabel::try_from).collect()
    }

    /// Every path carrying `label` across all stores in a workspace, most-recent
    /// first, bounded to `limit` (floored at 1) — the label filter.
    pub async fn list_by_label(
        &self,
        workspace_id: WorkspaceId,
        label: &str,
        limit: i64,
    ) -> Result<Vec<ObjectLabel>> {
        let rows: Vec<ObjectLabelRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {OBJECT_LABEL_COLS} FROM object_labels
             WHERE workspace_id = $1 AND label = $2
             ORDER BY created_at DESC, id ASC
             LIMIT $3"
        ))
        .bind(workspace_id.into_uuid())
        .bind(label)
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(ObjectLabel::try_from).collect()
    }

    /// Every label on any of the given `(store, path)` pairs in **one** query,
    /// workspace-scoped (SOUL §9/§18) — the batched form of
    /// [`list_for`](Self::list_for), so a page of object summaries can carry its
    /// label sets without an N+1. A pair with no labels is simply absent from
    /// the result; empty `pairs` → empty (no query). Oldest-first per the
    /// `list_for` order (then id for a stable tie-break).
    #[cfg(not(feature = "sqlite"))]
    pub async fn list_for_paths(
        &self,
        workspace_id: WorkspaceId,
        pairs: &[(String, String)],
    ) -> Result<Vec<ObjectLabel>> {
        if pairs.is_empty() {
            return Ok(Vec::new());
        }
        let (stores, paths): (Vec<String>, Vec<String>) = pairs.iter().cloned().unzip();
        let rows: Vec<ObjectLabelRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {OBJECT_LABEL_COLS} FROM object_labels
             WHERE workspace_id = $1
               AND (store, path) IN
                   (SELECT s, p FROM unnest($2::text[], $3::text[]) AS t(s, p))
             ORDER BY created_at ASC, id ASC"
        ))
        .bind(workspace_id.into_uuid())
        .bind(&stores)
        .bind(&paths)
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(ObjectLabel::try_from).collect()
    }

    #[cfg(feature = "sqlite")]
    pub async fn list_for_paths(
        &self,
        workspace_id: WorkspaceId,
        pairs: &[(String, String)],
    ) -> Result<Vec<ObjectLabel>> {
        if pairs.is_empty() {
            return Ok(Vec::new());
        }
        let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(format!(
            "SELECT {OBJECT_LABEL_COLS} FROM object_labels WHERE workspace_id = "
        ));
        query.push_bind(workspace_id.into_uuid()).push(" AND (");
        {
            let mut clauses = query.separated(" OR ");
            for (store, path) in pairs {
                clauses
                    .push("(store = ")
                    .push_bind(store)
                    .push(" AND path = ")
                    .push_bind(path)
                    .push_unseparated(")");
            }
        }
        query.push(") ORDER BY created_at ASC, id ASC");
        let rows: Vec<ObjectLabelRow> = query
            .build_query_as()
            .fetch_all(&self.pool)
            .await
            .map_err(map)?;
        rows.into_iter().map(ObjectLabel::try_from).collect()
    }

    /// Delete a label by id, workspace-scoped. [`StoreError::NotFound`] if absent.
    pub async fn delete(&self, workspace_id: WorkspaceId, id: ObjectLabelId) -> Result<()> {
        let res = sqlx::query("DELETE FROM object_labels WHERE id = $1 AND workspace_id = $2")
            .bind(id.into_uuid())
            .bind(workspace_id.into_uuid())
            .execute(&self.pool)
            .await
            .map_err(map)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// Purge every label on one `(store, path)` — used to keep labels in sync when
    /// a file's bytes are deleted so a label can't outlive its file. Idempotent:
    /// returns how many rows were removed (0 when nothing was labelled).
    pub async fn delete_for_path(
        &self,
        workspace_id: WorkspaceId,
        store: &str,
        path: &str,
    ) -> Result<u64> {
        let res = sqlx::query(
            "DELETE FROM object_labels
             WHERE workspace_id = $1 AND store = $2 AND path = $3",
        )
        .bind(workspace_id.into_uuid())
        .bind(store)
        .bind(path)
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(res.rows_affected())
    }
}
