use catalerum_core::id::{ConnectionId, EmailId, MailboxId, WorkspaceId};
use catalerum_core::model::{Attachment, Email, Mailbox};
use sqlx::types::Json;
#[cfg(not(feature = "sqlite"))]
use uuid::Uuid;
use crate::error::Result;
use crate::rows::{EmailRow, MailboxRow};
use super::{map, PgPool};

// ===========================================================================
// Mailboxes & emails (read-only email ingest, SOUL §28)
// ===========================================================================

const MAILBOX_COLS: &str =
    "id, workspace_id, connection_id, external_id, name, read_only, created_at";

/// CRUD for the `mailboxes` table (SOUL §6.1/§28). A mailbox belongs to an
/// email-kind [`Connection`](catalerum_core::model::Connection); `upsert` is keyed on `(connection_id, external_id)`
/// so re-listing the provider's folders never duplicates (mirrors `calendars`).
#[derive(Clone, Debug)]
pub struct MailboxRepo {
    pool: PgPool,
}

impl MailboxRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Upsert a mailbox by `(connection_id, external_id)`. On conflict the `name`
    /// and `read_only` flag are refreshed; the id is preserved. Idempotent (§3.4).
    pub async fn upsert(
        &self,
        workspace_id: WorkspaceId,
        connection_id: ConnectionId,
        external_id: &str,
        name: &str,
        read_only: bool,
    ) -> Result<Mailbox> {
        let id = MailboxId::new().into_uuid();
        let row: MailboxRow = sqlx::query_as(assert_sql_safe!(
            "INSERT INTO mailboxes (id, workspace_id, connection_id, external_id, name, read_only)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (connection_id, external_id)
             DO UPDATE SET name = EXCLUDED.name, read_only = EXCLUDED.read_only
             RETURNING {MAILBOX_COLS}"
        ))
        .bind(id)
        .bind(workspace_id.into_uuid())
        .bind(connection_id.into_uuid())
        .bind(external_id)
        .bind(name)
        .bind(read_only)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch a mailbox, scoped to its workspace.
    pub async fn get(&self, workspace_id: WorkspaceId, id: MailboxId) -> Result<Mailbox> {
        let row: MailboxRow = sqlx::query_as(assert_sql_safe!(
            "SELECT {MAILBOX_COLS} FROM mailboxes WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch many mailboxes by id in **one** query, workspace-scoped — the batched
    /// form of [`get`](Self::get) (mirrors `EmailRepo`/`WorkspaceRepo::get_many`).
    /// Used by `search_emails` to resolve just the mailboxes its hits reference
    /// without listing every mailbox in the workspace. An id absent in this
    /// workspace is silently omitted; empty `ids` → empty (no query).
    #[cfg(not(feature = "sqlite"))]
    pub async fn get_many(
        &self,
        workspace_id: WorkspaceId,
        ids: &[MailboxId],
    ) -> Result<Vec<Mailbox>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let uuids: Vec<Uuid> = ids.iter().map(MailboxId::as_uuid).collect();
        let rows: Vec<MailboxRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {MAILBOX_COLS} FROM mailboxes WHERE workspace_id = $1 AND id = ANY($2)"
        ))
        .bind(workspace_id.into_uuid())
        .bind(&uuids)
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Mailbox::from).collect())
    }

    #[cfg(feature = "sqlite")]
    pub async fn get_many(
        &self,
        workspace_id: WorkspaceId,
        ids: &[MailboxId],
    ) -> Result<Vec<Mailbox>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(format!(
            "SELECT {MAILBOX_COLS} FROM mailboxes WHERE workspace_id = "
        ));
        query
            .push_bind(workspace_id.into_uuid())
            .push(" AND id IN (");
        let mut values = query.separated(", ");
        for id in ids {
            values.push_bind(id.as_uuid());
        }
        values.push_unseparated(")");
        let rows: Vec<MailboxRow> = query
            .build_query_as()
            .fetch_all(&self.pool)
            .await
            .map_err(map)?;
        Ok(rows.into_iter().map(Mailbox::from).collect())
    }

    /// List a workspace's mailboxes.
    pub async fn list_by_workspace(&self, workspace_id: WorkspaceId) -> Result<Vec<Mailbox>> {
        let rows: Vec<MailboxRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {MAILBOX_COLS} FROM mailboxes WHERE workspace_id = $1 ORDER BY name ASC"
        ))
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Mailbox::from).collect())
    }
}

/// The full set of `emails` columns, in row order.
const EMAIL_COLS: &str = "id, workspace_id, mailbox_id, uid, message_id, from_addr, to_addrs, \
     cc_addrs, subject, received_at, body_text, body_html, has_attachments, flags, labels, raw_ref, \
     attachments, created_at, updated_at";

/// CRUD for the `emails` table (SOUL §6.1/§28/§10). Emails upsert by
/// `(mailbox_id, uid)` so incremental sync is idempotent (§3.4): re-running never
/// duplicates and edits (flag changes, re-fetch) land in place.
#[derive(Clone, Debug)]
pub struct EmailRepo {
    pool: PgPool,
}

impl EmailRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Insert or update an email by `(mailbox_id, uid)`. On conflict every mutable
    /// field is refreshed and `updated_at` bumped; the row id is preserved. The
    /// passed `email`'s `id` is used only on first insert. Returns the stored email.
    pub async fn upsert_by_uid(&self, email: &Email) -> Result<Email> {
        let row: EmailRow = sqlx::query_as(assert_sql_safe!(
            "INSERT INTO emails
                 (id, workspace_id, mailbox_id, uid, message_id, from_addr, to_addrs, cc_addrs,
                  subject, received_at, body_text, body_html, has_attachments, flags)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
             ON CONFLICT (mailbox_id, uid) DO UPDATE SET
                 message_id      = EXCLUDED.message_id,
                 from_addr       = EXCLUDED.from_addr,
                 to_addrs        = EXCLUDED.to_addrs,
                 cc_addrs        = EXCLUDED.cc_addrs,
                 subject         = EXCLUDED.subject,
                 received_at     = EXCLUDED.received_at,
                 body_text       = EXCLUDED.body_text,
                 body_html       = EXCLUDED.body_html,
                 has_attachments = EXCLUDED.has_attachments,
                 flags           = EXCLUDED.flags,
                 updated_at      = CURRENT_TIMESTAMP
             RETURNING {EMAIL_COLS}"
        ))
        .bind(email.id.into_uuid())
        .bind(email.workspace_id.into_uuid())
        .bind(email.mailbox_id.into_uuid())
        .bind(&email.uid)
        .bind(email.message_id.as_deref())
        .bind(Json(email.from.clone()))
        .bind(Json(email.to.clone()))
        .bind(Json(email.cc.clone()))
        .bind(&email.subject)
        .bind(email.received_at)
        .bind(email.body_text.as_deref())
        .bind(email.body_html.as_deref())
        .bind(email.has_attachments)
        .bind(Json(email.flags.clone()))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Record the object-storage key of an email's archived raw message
    /// (`mail/<id>.eml`, SOUL §9/§28). Set after the raw bytes are written to the
    /// storage backend; kept out of [`upsert_by_uid`] so a flag-only re-sync (which
    /// does not know the key) never clobbers an existing ref. A no-op if the email
    /// is absent.
    pub async fn set_raw_ref(
        &self,
        workspace_id: WorkspaceId,
        id: EmailId,
        raw_ref: &str,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE emails SET raw_ref = $1, updated_at = CURRENT_TIMESTAMP WHERE id = $2 AND workspace_id = $3",
        )
        .bind(raw_ref)
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(())
    }

    /// Record an email's archived attachment **references** (SOUL §9/§28/§29): the
    /// list of [`Attachment`]s pointing at the objects a `WriteEmail`-triggered
    /// archival wrote to the workspace's files store. Like [`set_raw_ref`], kept out
    /// of [`upsert_by_uid`] so a flag-only re-sync (which carries no refs) never
    /// clobbers them; a full replace, idempotent. A no-op if the email is absent.
    pub async fn set_attachments(
        &self,
        workspace_id: WorkspaceId,
        id: EmailId,
        attachments: &[Attachment],
    ) -> Result<()> {
        sqlx::query(
            "UPDATE emails SET attachments = $1, updated_at = CURRENT_TIMESTAMP \
             WHERE id = $2 AND workspace_id = $3",
        )
        .bind(Json(attachments.to_vec()))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(())
    }

    /// Replace an email's free-text `labels` (SOUL §11/§28) — the classifier verdict
    /// a `LabelEmail` automation action records. Idempotent (a full replace), and
    /// kept out of [`upsert_by_uid`] so a provider re-sync (which carries no verdict)
    /// never clobbers a label set. Returns the updated email; `StoreError::NotFound`
    /// if the email is absent (so a verdict for an unwritten message surfaces rather
    /// than silently no-op'ing).
    pub async fn set_labels(
        &self,
        workspace_id: WorkspaceId,
        id: EmailId,
        labels: &[String],
    ) -> Result<Email> {
        let row: EmailRow = sqlx::query_as(assert_sql_safe!(
            "UPDATE emails SET labels = $1, updated_at = CURRENT_TIMESTAMP \
             WHERE id = $2 AND workspace_id = $3 RETURNING {EMAIL_COLS}"
        ))
        .bind(Json(labels.to_vec()))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Set or clear an email's local `seen` flag (SOUL §28) — the read/unread
    /// state the workbench and the `MarkEmailRead` automation action mutate.
    /// **Local only**: catalerum never writes back to the provider (§14), so a
    /// provider re-sync that carries fresh flags may overwrite this. One
    /// statement (strip any case-variant `seen` token, then append the
    /// normalized one when marking read), so it is idempotent and never
    /// duplicates the token. Returns the updated email; `StoreError::NotFound`
    /// if the email is absent.
    pub async fn set_seen(
        &self,
        workspace_id: WorkspaceId,
        id: EmailId,
        seen: bool,
    ) -> Result<Email> {
        let row: EmailRow = sqlx::query_as(assert_sql_safe!(
            "UPDATE emails SET flags =
                 (SELECT COALESCE(jsonb_agg(tok), '[]'::jsonb)
                  FROM jsonb_array_elements_text(flags) AS tok
                  WHERE lower(tok) <> 'seen')
                 || CASE WHEN $3 THEN '[\"seen\"]'::jsonb ELSE '[]'::jsonb END,
             updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2 RETURNING {EMAIL_COLS}"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(seen)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch an email by id, workspace-scoped.
    pub async fn get(&self, workspace_id: WorkspaceId, id: EmailId) -> Result<Email> {
        let row: EmailRow = sqlx::query_as(assert_sql_safe!(
            "SELECT {EMAIL_COLS} FROM emails WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch many emails by id in **one** query, workspace-scoped — the batched form
    /// of [`get`](Self::get), used by `search_emails` to resolve a page of vector
    /// hits without an N+1 (was a `get` per hit). An id absent in this workspace (a
    /// foreign tenant's id, or one whose row was purged after the vector outlived it)
    /// is **silently omitted**, so the result may be shorter than `ids` and in
    /// arbitrary order — the caller maps by id. Empty `ids` → empty (no query).
    #[cfg(not(feature = "sqlite"))]
    pub async fn get_many(&self, workspace_id: WorkspaceId, ids: &[EmailId]) -> Result<Vec<Email>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let uuids: Vec<Uuid> = ids.iter().map(EmailId::as_uuid).collect();
        let rows: Vec<EmailRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {EMAIL_COLS} FROM emails WHERE workspace_id = $1 AND id = ANY($2)"
        ))
        .bind(workspace_id.into_uuid())
        .bind(&uuids)
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Email::from).collect())
    }

    #[cfg(feature = "sqlite")]
    pub async fn get_many(&self, workspace_id: WorkspaceId, ids: &[EmailId]) -> Result<Vec<Email>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(format!(
            "SELECT {EMAIL_COLS} FROM emails WHERE workspace_id = "
        ));
        query
            .push_bind(workspace_id.into_uuid())
            .push(" AND id IN (");
        let mut values = query.separated(", ");
        for id in ids {
            values.push_bind(id.as_uuid());
        }
        values.push_unseparated(")");
        let rows: Vec<EmailRow> = query
            .build_query_as()
            .fetch_all(&self.pool)
            .await
            .map_err(map)?;
        Ok(rows.into_iter().map(Email::from).collect())
    }

    /// Fetch an email by `(mailbox_id, uid)`, workspace-scoped.
    pub async fn get_by_uid(
        &self,
        workspace_id: WorkspaceId,
        mailbox_id: MailboxId,
        uid: &str,
    ) -> Result<Email> {
        let row: EmailRow = sqlx::query_as(assert_sql_safe!(
            "SELECT {EMAIL_COLS} FROM emails
             WHERE workspace_id = $1 AND mailbox_id = $2 AND uid = $3"
        ))
        .bind(workspace_id.into_uuid())
        .bind(mailbox_id.into_uuid())
        .bind(uid)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// List a workspace's emails, most-recently-received first (nulls last).
    pub async fn list_by_workspace(
        &self,
        workspace_id: WorkspaceId,
        limit: i64,
    ) -> Result<Vec<Email>> {
        let rows: Vec<EmailRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {EMAIL_COLS} FROM emails
             WHERE workspace_id = $1
             ORDER BY received_at DESC NULLS LAST, id ASC
             LIMIT $2"
        ))
        .bind(workspace_id.into_uuid())
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Email::from).collect())
    }

    /// List a workspace's emails carrying **no labels** (SOUL §11/§28) —
    /// most-recently-received first (nulls last). The backlog feed for a
    /// scheduled "classify untagged mail" automation: filtered in SQL (`labels
    /// = '[]'`, total since migration 0028 makes the column NOT NULL DEFAULT
    /// '[]') so untagged mail older than any recent-scan window is still
    /// reachable, and already-labelled messages never crowd the page.
    pub async fn list_untagged_by_workspace(
        &self,
        workspace_id: WorkspaceId,
        limit: i64,
    ) -> Result<Vec<Email>> {
        let rows: Vec<EmailRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {EMAIL_COLS} FROM emails
             WHERE workspace_id = $1 AND labels = '[]'::jsonb
             ORDER BY received_at DESC NULLS LAST, id ASC
             LIMIT $2"
        ))
        .bind(workspace_id.into_uuid())
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Email::from).collect())
    }

    /// List a single mailbox's emails, most-recently-received first.
    pub async fn list_by_mailbox(
        &self,
        workspace_id: WorkspaceId,
        mailbox_id: MailboxId,
        limit: i64,
    ) -> Result<Vec<Email>> {
        let rows: Vec<EmailRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {EMAIL_COLS} FROM emails
             WHERE workspace_id = $1 AND mailbox_id = $2
             ORDER BY received_at DESC NULLS LAST, id ASC
             LIMIT $3"
        ))
        .bind(workspace_id.into_uuid())
        .bind(mailbox_id.into_uuid())
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Email::from).collect())
    }

    /// List every email row sharing an RFC 5322 `message_id`, workspace-scoped
    /// (SOUL §29 — **cross-folder dedup**). With per-folder mailboxes the same logical
    /// message lands as one row per folder it appears in (each keyed by its own
    /// `(mailbox_id, uid)`, since deletion/flags are per-folder); this collapses those
    /// N folder-copies by their shared `Message-ID` so a caller — recall/search
    /// dedup, or the web grouping a thread — can treat them as one logical email.
    /// Served by the `emails_message_id_idx` index over `(workspace_id, message_id)`.
    /// A blank id never groups (such rows store `NULL`); an empty match → empty.
    /// Most-recently-received first (nulls last), then `id` for a stable tie-break.
    pub async fn list_by_message_id(
        &self,
        workspace_id: WorkspaceId,
        message_id: &str,
    ) -> Result<Vec<Email>> {
        let rows: Vec<EmailRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {EMAIL_COLS} FROM emails
             WHERE workspace_id = $1 AND message_id = $2
             ORDER BY received_at DESC NULLS LAST, id ASC"
        ))
        .bind(workspace_id.into_uuid())
        .bind(message_id)
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Email::from).collect())
    }

    /// For each of `message_ids`, the DISTINCT mailbox **names** a message with that
    /// RFC 5322 `Message-ID` is filed under, workspace-scoped — the page-scoped group
    /// query behind the inbox listing's cross-folder dedup annotation (SOUL §29). ONE
    /// query for the whole listed page (never one-per-row): the caller passes the page's
    /// distinct non-null `message_id`s and gets back, per id, the set of folders that
    /// message appears in, so each row can be annotated "also in N other folders" without
    /// an N+1. A message filed in a single folder maps to a one-element vec; an id with no
    /// rows is absent from the map. Names are de-duplicated and returned in ascending
    /// order. Empty input → empty (no query). Served by the `emails_message_id_idx` index
    /// over `(workspace_id, message_id)`.
    #[cfg(not(feature = "sqlite"))]
    pub async fn folders_by_message_id(
        &self,
        workspace_id: WorkspaceId,
        message_ids: &[String],
    ) -> Result<std::collections::HashMap<String, Vec<String>>> {
        if message_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT DISTINCT e.message_id, m.name
             FROM emails e
             JOIN mailboxes m ON m.id = e.mailbox_id AND m.workspace_id = e.workspace_id
             WHERE e.workspace_id = $1 AND e.message_id = ANY($2)
             ORDER BY e.message_id, m.name",
        )
        .bind(workspace_id.into_uuid())
        .bind(message_ids)
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        let mut out: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for (mid, name) in rows {
            out.entry(mid).or_default().push(name);
        }
        Ok(out)
    }

    #[cfg(feature = "sqlite")]
    pub async fn folders_by_message_id(
        &self,
        workspace_id: WorkspaceId,
        message_ids: &[String],
    ) -> Result<std::collections::HashMap<String, Vec<String>>> {
        if message_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
            "SELECT DISTINCT e.message_id, m.name FROM emails e \
             JOIN mailboxes m ON m.id = e.mailbox_id AND m.workspace_id = e.workspace_id \
             WHERE e.workspace_id = ",
        );
        query
            .push_bind(workspace_id.into_uuid())
            .push(" AND e.message_id IN (");
        let mut values = query.separated(", ");
        for id in message_ids {
            values.push_bind(id);
        }
        values.push_unseparated(") ORDER BY e.message_id, m.name");
        let rows: Vec<(String, String)> = query
            .build_query_as()
            .fetch_all(&self.pool)
            .await
            .map_err(map)?;
        let mut out: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for (mid, name) in rows {
            out.entry(mid).or_default().push(name);
        }
        Ok(out)
    }

    /// Per-mailbox **unread counts** for a whole workspace in ONE grouped query
    /// (SOUL §28) — the badge numbers next to each mailbox in the inbox sidebar.
    /// Unread mirrors `is_unread` (no case-insensitive `seen` flag). A mailbox
    /// with no unread mail is absent from the map (the caller treats missing as
    /// `0`).
    pub async fn unread_counts_by_mailbox(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<std::collections::HashMap<MailboxId, i64>> {
        let rows: Vec<(uuid::Uuid, i64)> = sqlx::query_as(
            "SELECT mailbox_id, count(*) FROM emails
             WHERE workspace_id = $1
               AND NOT EXISTS (
                   SELECT 1 FROM jsonb_array_elements_text(flags) AS tok
                   WHERE lower(tok) = 'seen'
               )
             GROUP BY mailbox_id",
        )
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows
            .into_iter()
            .map(|(id, n)| (MailboxId::from_uuid(id), n))
            .collect())
    }

    /// Search a workspace's emails by optional `content` (subject/body substring),
    /// `sender` (from address/name substring), and `unread` state, optionally scoped
    /// to one `mailbox`, most-recently-received first, bounded to `limit`.
    ///
    /// Every predicate is applied **in SQL** so the `LIMIT` bounds *matching* rows —
    /// unlike a scan-then-filter, a match that lives in older-than-`limit` mail is
    /// still found (SOUL §28/§18). Substring matching is case-insensitive and
    /// **literal** (`strpos`, not `ILIKE`, so a `%`/`_` in the term isn't a
    /// wildcard), mirroring the route's previous `contains` filter. The `unread`
    /// predicate mirrors `is_unread` (true ⟺ no case-insensitive `seen` flag).
    /// `None` for any predicate means "no constraint".
    pub async fn search_in_workspace(
        &self,
        workspace_id: WorkspaceId,
        mailbox: Option<MailboxId>,
        content: Option<&str>,
        sender: Option<&str>,
        unread: Option<bool>,
        limit: i64,
    ) -> Result<Vec<Email>> {
        let rows: Vec<EmailRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {EMAIL_COLS} FROM emails
             WHERE workspace_id = $1
               AND ($2::uuid IS NULL OR mailbox_id = $2)
               AND ($3::text IS NULL
                    OR strpos(lower(subject), lower($3)) > 0
                    OR strpos(lower(coalesce(body_text, '')), lower($3)) > 0)
               AND ($4::text IS NULL
                    OR strpos(lower(coalesce(from_addr->>'address', '')), lower($4)) > 0
                    OR strpos(lower(coalesce(from_addr->>'name', '')), lower($4)) > 0)
               AND ($5::bool IS NULL
                    OR (NOT EXISTS (
                        SELECT 1 FROM jsonb_array_elements_text(flags) AS tok
                        WHERE lower(tok) = 'seen'
                    )) = $5)
             ORDER BY received_at DESC NULLS LAST, id ASC
             LIMIT $6"
        ))
        .bind(workspace_id.into_uuid())
        .bind(mailbox.map(MailboxId::into_uuid))
        .bind(content)
        .bind(sender)
        .bind(unread)
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Email::from).collect())
    }

    /// Delete an email by `(mailbox_id, uid)`, workspace-scoped. Returns the
    /// deleted email's id (so sync can enqueue a projection purge for it, §10) or
    /// `None` if no row matched. Idempotent.
    pub async fn delete_by_uid(
        &self,
        workspace_id: WorkspaceId,
        mailbox_id: MailboxId,
        uid: &str,
    ) -> Result<Option<EmailId>> {
        let row: Option<(uuid::Uuid,)> = sqlx::query_as(
            "DELETE FROM emails WHERE workspace_id = $1 AND mailbox_id = $2 AND uid = $3
             RETURNING id",
        )
        .bind(workspace_id.into_uuid())
        .bind(mailbox_id.into_uuid())
        .bind(uid)
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.map(|(id,)| EmailId::from_uuid(id)))
    }
}
