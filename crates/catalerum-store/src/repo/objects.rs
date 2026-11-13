use catalerum_core::id::{BucketId, ConnectionId, DocumentId, ObjectId, WorkspaceId};
use catalerum_core::model::{Bucket, StoredObject};
use chrono::{DateTime, Utc};
use uuid::Uuid;
use crate::error::Result;
use crate::rows::{BucketRow, ObjectRow};
use super::{map, PgPool};

// ===========================================================================
// Buckets & objects (the storage catalogue, SOUL §9)
// ===========================================================================

/// The full set of `buckets` columns, in row order.
const BUCKET_COLS: &str = "id, workspace_id, connection_id, name, prefix, created_at";

/// CRUD for the `buckets` table (SOUL §6.1/§9). A bucket belongs to a
/// storage-kind [`Connection`](catalerum_core::model::Connection) (mirrors calendars on a calendar connection);
/// [`ensure`](BucketRepo::ensure) is the get-or-create the storage REST surface
/// calls before cataloguing an upload.
#[derive(Clone, Debug)]
pub struct BucketRepo {
    pool: PgPool,
}

impl BucketRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Get-or-create a bucket by `(connection_id, name)`. Idempotent: on conflict
    /// the existing row (id preserved) is returned with its `prefix` refreshed, so
    /// repeatedly cataloguing into the same configured bucket never duplicates.
    pub async fn ensure(
        &self,
        workspace_id: WorkspaceId,
        connection_id: ConnectionId,
        name: &str,
        prefix: Option<&str>,
    ) -> Result<Bucket> {
        let id = BucketId::new().into_uuid();
        let row: BucketRow = sqlx::query_as(assert_sql_safe!(
            "INSERT INTO buckets (id, workspace_id, connection_id, name, prefix)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (connection_id, name)
             DO UPDATE SET prefix = EXCLUDED.prefix
             RETURNING {BUCKET_COLS}"
        ))
        .bind(id)
        .bind(workspace_id.into_uuid())
        .bind(connection_id.into_uuid())
        .bind(name)
        .bind(prefix)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch a bucket, scoped to its workspace.
    pub async fn get(&self, workspace_id: WorkspaceId, id: BucketId) -> Result<Bucket> {
        let row: BucketRow = sqlx::query_as(assert_sql_safe!(
            "SELECT {BUCKET_COLS} FROM buckets WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch a bucket by its `name`, scoped to its workspace. The name is the
    /// catalogue label a `StorageObject` trigger carries as `bucket`, so this is
    /// how a storage-change automation resolves a triggered file back to its
    /// bucket. Names are expected unique per workspace (the newest wins if not).
    pub async fn get_by_name(&self, workspace_id: WorkspaceId, name: &str) -> Result<Bucket> {
        let row: BucketRow = sqlx::query_as(assert_sql_safe!(
            "SELECT {BUCKET_COLS} FROM buckets WHERE workspace_id = $1 AND name = $2 \
             ORDER BY created_at DESC LIMIT 1"
        ))
        .bind(workspace_id.into_uuid())
        .bind(name)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// List a workspace's buckets, newest first.
    pub async fn list_by_workspace(&self, workspace_id: WorkspaceId) -> Result<Vec<Bucket>> {
        let rows: Vec<BucketRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {BUCKET_COLS} FROM buckets WHERE workspace_id = $1 ORDER BY created_at DESC"
        ))
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Bucket::from).collect())
    }
}

/// The full set of `objects` columns, in row order.
const OBJECT_COLS: &str = "id, workspace_id, bucket_id, key, size, content_type, etag, \
     last_modified, sha256, extracted_text_id, created_at, updated_at";

/// Default ceiling on how many catalogued objects one
/// [`ObjectRepo::list_by_workspace`] call returns. A bucket catalogued from an
/// external store can hold many thousands of objects; an unbounded list would
/// balloon the API payload. The list is bounded to this many (most-recently-
/// modified first); narrow it with a key `prefix`. Normal-sized catalogues fall
/// well under it and are unaffected.
pub const DEFAULT_OBJECT_LIMIT: i64 = 1000;

/// Default cap on [`ObjectRepo::search_text_in_workspace`] results.
pub const DEFAULT_OBJECT_SEARCH_LIMIT: i64 = 50;

/// One object-content search hit: which object matched + a short excerpt of its
/// §10 extracted text windowed around the match.
#[derive(Clone, Debug)]
pub struct ObjectTextHit {
    pub id: ObjectId,
    /// The bucket the object lives in — lets a caller resolve the `?store=` the
    /// hit should download from (content search spans every store, §9).
    pub bucket_id: BucketId,
    pub key: String,
    pub content_type: Option<String>,
    pub excerpt: String,
}

/// New/updated object metadata for [`ObjectRepo::upsert`]. The catalogued handle
/// for a stored blob; the bytes themselves live in the bucket (§14).
#[derive(Debug, Clone)]
pub struct UpsertObject<'a> {
    pub workspace_id: WorkspaceId,
    pub bucket_id: BucketId,
    pub key: &'a str,
    pub size: u64,
    pub content_type: Option<&'a str>,
    pub etag: Option<&'a str>,
    pub last_modified: DateTime<Utc>,
    pub sha256: Option<&'a str>,
}

/// CRUD for the `objects` table (SOUL §6.1/§9/§10) — the catalogued, queryable
/// metadata for stored objects. `upsert` is keyed on `(bucket_id, key)` so a
/// re-upload refreshes metadata and never duplicates (§3.4); blobs stay in the
/// bucket. Powers `query_structured`'s object lookups (§6.5) and later §10
/// chunk/embed/project ingestion (via `extracted_text_id`).
#[derive(Clone, Debug)]
pub struct ObjectRepo {
    pool: PgPool,
}

impl ObjectRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Catalogue (or re-catalogue) an object by `(bucket_id, key)`. Idempotent:
    /// on conflict, `size`/`content_type`/`etag`/`last_modified`/`sha256` are
    /// refreshed and the id is preserved (`extracted_text_id` is left untouched,
    /// so a re-upload doesn't drop the §10 ingest link).
    pub async fn upsert(&self, obj: &UpsertObject<'_>) -> Result<StoredObject> {
        let id = ObjectId::new().into_uuid();
        let row: ObjectRow = sqlx::query_as(assert_sql_safe!(
            "INSERT INTO objects
                 (id, workspace_id, bucket_id, key, size, content_type, etag, last_modified, sha256)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT (bucket_id, key) DO UPDATE SET
                 size = EXCLUDED.size,
                 content_type = EXCLUDED.content_type,
                 etag = EXCLUDED.etag,
                 last_modified = EXCLUDED.last_modified,
                 sha256 = EXCLUDED.sha256,
                 updated_at = CURRENT_TIMESTAMP
             RETURNING {OBJECT_COLS}"
        ))
        .bind(id)
        .bind(obj.workspace_id.into_uuid())
        .bind(obj.bucket_id.into_uuid())
        .bind(obj.key)
        .bind(obj.size as i64)
        .bind(obj.content_type)
        .bind(obj.etag)
        .bind(obj.last_modified)
        .bind(obj.sha256)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch a catalogued object by id, workspace-scoped.
    pub async fn get(&self, workspace_id: WorkspaceId, id: ObjectId) -> Result<StoredObject> {
        let row: ObjectRow = sqlx::query_as(assert_sql_safe!(
            "SELECT {OBJECT_COLS} FROM objects WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Link (or clear) the [`Document`](catalerum_core::model::Document) holding this object's extracted text
    /// (§10), workspace-scoped. Idempotent; returns the updated object.
    pub async fn set_extracted_text(
        &self,
        workspace_id: WorkspaceId,
        id: ObjectId,
        document_id: Option<DocumentId>,
    ) -> Result<StoredObject> {
        let row: ObjectRow = sqlx::query_as(assert_sql_safe!(
            "UPDATE objects SET extracted_text_id = $3, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2
             RETURNING {OBJECT_COLS}"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(document_id.map(DocumentId::into_uuid))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch a catalogued object by `(bucket_id, key)`, workspace-scoped.
    pub async fn get_by_key(
        &self,
        workspace_id: WorkspaceId,
        bucket_id: BucketId,
        key: &str,
    ) -> Result<StoredObject> {
        let row: ObjectRow = sqlx::query_as(assert_sql_safe!(
            "SELECT {OBJECT_COLS} FROM objects
             WHERE workspace_id = $1 AND bucket_id = $2 AND key = $3"
        ))
        .bind(workspace_id.into_uuid())
        .bind(bucket_id.into_uuid())
        .bind(key)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// List a workspace's catalogued objects, most-recently-modified first.
    /// List the workspace's catalogued objects, most-recently-modified first,
    /// optionally restricted to keys under `prefix` (a *literal* prefix — LIKE
    /// metacharacters in it are not special — empty = all). Bounded to `limit`
    /// rows (floored at 1, see [`DEFAULT_OBJECT_LIMIT`]) so a large catalogue
    /// can't return an unbounded set; the bound applies *after* the prefix
    /// filter, so you get the first `limit` matches, not `limit`-then-filtered.
    pub async fn list_by_workspace(
        &self,
        workspace_id: WorkspaceId,
        prefix: &str,
        limit: i64,
    ) -> Result<Vec<StoredObject>> {
        let rows: Vec<ObjectRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {OBJECT_COLS} FROM objects
             WHERE workspace_id = $1
               AND ($2 = '' OR starts_with(key, $2))
             ORDER BY last_modified DESC, key ASC
             LIMIT $3"
        ))
        .bind(workspace_id.into_uuid())
        .bind(prefix)
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(StoredObject::from).collect())
    }

    /// List the workspace's catalogued objects carrying **no labels** (SOUL
    /// §9/§11) — the storage twin of `EmailRepo::list_untagged_by_workspace`:
    /// the backlog feed for a scheduled "label the unlabelled files" sweep.
    /// Labels live in `object_labels` keyed by `(store, path)` while objects key
    /// by `(bucket_id, key)`, and the bucket→store-name mapping is config (the
    /// storage registry), not DB — so the caller passes it in as `bucket_stores`
    /// and the anti-join runs per bucket under its own store name. Filtered in
    /// SQL (`NOT EXISTS`) so old unlabelled files stay reachable however many
    /// labelled ones are newer; `prefix` optionally restricts to keys under a
    /// subdirectory (a *literal* prefix — empty = everywhere). Only **exact**
    /// path labels count: a label on an ancestor directory does not mark the
    /// files under it. Most-recently-modified first, bounded to `limit` (floored
    /// at 1); empty `bucket_stores` → empty (no query).
    #[cfg(not(feature = "sqlite"))]
    pub async fn list_unlabeled_by_workspace(
        &self,
        workspace_id: WorkspaceId,
        bucket_stores: &[(BucketId, String)],
        prefix: &str,
        limit: i64,
    ) -> Result<Vec<StoredObject>> {
        if bucket_stores.is_empty() {
            return Ok(Vec::new());
        }
        let (buckets, stores): (Vec<Uuid>, Vec<String>) = bucket_stores
            .iter()
            .map(|(b, s)| (b.into_uuid(), s.clone()))
            .unzip();
        let rows: Vec<ObjectRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {OBJECT_COLS} FROM objects o
             JOIN unnest($2::uuid[], $3::text[]) AS m(b_id, store_name)
               ON m.b_id = o.bucket_id
             WHERE o.workspace_id = $1
               AND ($4 = '' OR starts_with(o.key, $4))
               AND NOT EXISTS (
                   SELECT 1 FROM object_labels l
                   WHERE l.workspace_id = o.workspace_id
                     AND l.store = m.store_name
                     AND l.path = o.key)
             ORDER BY o.last_modified DESC, o.key ASC
             LIMIT $5"
        ))
        .bind(workspace_id.into_uuid())
        .bind(&buckets)
        .bind(&stores)
        .bind(prefix)
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(StoredObject::from).collect())
    }

    #[cfg(feature = "sqlite")]
    pub async fn list_unlabeled_by_workspace(
        &self,
        workspace_id: WorkspaceId,
        bucket_stores: &[(BucketId, String)],
        prefix: &str,
        limit: i64,
    ) -> Result<Vec<StoredObject>> {
        if bucket_stores.is_empty() {
            return Ok(Vec::new());
        }
        let mut query =
            sqlx::QueryBuilder::<sqlx::Sqlite>::new("WITH mapping(b_id, store_name) AS (VALUES ");
        {
            let mut rows = query.separated(", ");
            for (bucket, store) in bucket_stores {
                rows.push("(")
                    .push_bind(bucket.into_uuid())
                    .push(", ")
                    .push_bind(store)
                    .push_unseparated(")");
            }
        }
        query
            .push(format!(
                ") SELECT {OBJECT_COLS} FROM objects o \
                 JOIN mapping m ON m.b_id = o.bucket_id WHERE o.workspace_id = "
            ))
            .push_bind(workspace_id.into_uuid())
            .push(" AND (")
            .push_bind(prefix)
            .push(" = '' OR substr(o.key, 1, length(")
            .push_bind(prefix)
            .push(")) = ")
            .push_bind(prefix)
            .push(
                ") AND NOT EXISTS (SELECT 1 FROM object_labels l \
                   WHERE l.workspace_id = o.workspace_id AND l.store = m.store_name \
                     AND l.path = o.key) ORDER BY o.last_modified DESC, o.key ASC LIMIT ",
            )
            .push_bind(limit.max(1));
        let rows: Vec<ObjectRow> = query
            .build_query_as()
            .fetch_all(&self.pool)
            .await
            .map_err(map)?;
        Ok(rows.into_iter().map(StoredObject::from).collect())
    }

    /// List a single bucket's catalogued objects, by key.
    pub async fn list_by_bucket(
        &self,
        workspace_id: WorkspaceId,
        bucket_id: BucketId,
    ) -> Result<Vec<StoredObject>> {
        let rows: Vec<ObjectRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {OBJECT_COLS} FROM objects
             WHERE workspace_id = $1 AND bucket_id = $2
             ORDER BY key ASC"
        ))
        .bind(workspace_id.into_uuid())
        .bind(bucket_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(StoredObject::from).collect())
    }

    /// Search objects by the **content** of their §10 extracted text. Joins each
    /// object to its extracted-text document (`extracted_text_id`), matches `query`
    /// as a **literal case-insensitive substring** (`strpos` — not `LIKE`, so a
    /// user's `%`/`_` are literal), and returns a hit per object with a ~160-char
    /// **excerpt windowed around the match** (computed in SQL, so the payload stays
    /// small regardless of document size), newest-modified first. A blank query
    /// returns nothing; bounded by `limit` (floored at 1). Only ingested objects
    /// (those with extracted text) can match.
    pub async fn search_text_in_workspace(
        &self,
        workspace_id: WorkspaceId,
        query: &str,
        limit: i64,
    ) -> Result<Vec<ObjectTextHit>> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<(Uuid, Uuid, String, Option<String>, String)> = sqlx::query_as(
            "SELECT o.id, o.bucket_id, o.key, o.content_type,
                    substring(d.text FROM greatest(1, strpos(lower(d.text), lower($2)) - 60) FOR 160)
                        AS excerpt
             FROM objects o
             JOIN documents d ON d.id = o.extracted_text_id
             WHERE o.workspace_id = $1 AND strpos(lower(d.text), lower($2)) > 0
             ORDER BY o.last_modified DESC, o.key ASC
             LIMIT $3",
        )
        .bind(workspace_id.into_uuid())
        .bind(query)
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows
            .into_iter()
            .map(
                |(id, bucket_id, key, content_type, excerpt)| ObjectTextHit {
                    id: ObjectId::from_uuid(id),
                    bucket_id: BucketId::from_uuid(bucket_id),
                    key,
                    content_type,
                    excerpt,
                },
            )
            .collect())
    }

    /// Remove a catalogued object by `(bucket_id, key)`, workspace-scoped.
    /// Idempotent: removing a never-catalogued / already-removed key is a no-op
    /// (mirrors the backend's idempotent delete).
    pub async fn delete_by_key(
        &self,
        workspace_id: WorkspaceId,
        bucket_id: BucketId,
        key: &str,
    ) -> Result<()> {
        sqlx::query("DELETE FROM objects WHERE workspace_id = $1 AND bucket_id = $2 AND key = $3")
            .bind(workspace_id.into_uuid())
            .bind(bucket_id.into_uuid())
            .bind(key)
            .execute(&self.pool)
            .await
            .map_err(map)?;
        Ok(())
    }
}
