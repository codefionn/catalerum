use catalerum_core::id::{ChunkId, DocumentId, WorkspaceId};
use catalerum_core::model::{Chunk, Document, SourceRef};
use uuid::Uuid;
use crate::error::{Result, StoreError};
use crate::rows::{ChunkRow, DocumentRow, source_to_parts};
use super::{map, PgPool};

// ===========================================================================
// Documents & chunks (ingest derivation, SOUL §5/§6.4/§10)
// ===========================================================================

/// The full set of `documents` columns, in row order (for `SELECT`/`RETURNING`).
const DOCUMENT_COLS: &str =
    "id, workspace_id, source_kind, source_id, text, summary, created_at, updated_at";

/// CRUD for the `documents` table — extracted text for one source artifact, the
/// unit that gets chunked + embedded (SOUL §10). Keyed on the core `SourceRef`
/// so ingesting the same source twice upserts one stable row.
#[derive(Clone, Debug)]
pub struct DocumentRepo {
    pool: PgPool,
}

impl DocumentRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Idempotently upsert the document for `source` in a workspace, replacing
    /// its `text`/`summary` and bumping `updated_at`. The document **id is
    /// stable** across upserts (the `(workspace_id, source_kind, source_id)`
    /// unique key), so a note's chunks/edges keep referring to the same document
    /// on every re-ingest (SOUL §3.4/§10).
    pub async fn upsert_by_source(
        &self,
        workspace_id: WorkspaceId,
        source: &SourceRef,
        text: &str,
        summary: Option<&str>,
    ) -> Result<Document> {
        let (kind, sid) = source_to_parts(source);
        let row: DocumentRow = sqlx::query_as(assert_sql_safe!(
            "INSERT INTO documents (id, workspace_id, source_kind, source_id, text, summary)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (workspace_id, source_kind, source_id)
             DO UPDATE SET text = EXCLUDED.text,
                           summary = EXCLUDED.summary,
                           updated_at = CURRENT_TIMESTAMP
             RETURNING {DOCUMENT_COLS}"
        ))
        .bind(DocumentId::new().into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(kind)
        .bind(sid)
        .bind(text)
        .bind(summary)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Fetch the document for `source`, or `None` if it has not been ingested.
    pub async fn get_by_source(
        &self,
        workspace_id: WorkspaceId,
        source: &SourceRef,
    ) -> Result<Option<Document>> {
        let (kind, sid) = source_to_parts(source);
        let row: Option<DocumentRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {DOCUMENT_COLS} FROM documents
             WHERE workspace_id = $1 AND source_kind = $2 AND source_id = $3"
        ))
        .bind(workspace_id.into_uuid())
        .bind(kind)
        .bind(sid)
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        row.map(Document::try_from).transpose()
    }

    /// Delete the document for `source` (cascading to its chunks), workspace-
    /// scoped. Returns whether a row was deleted (a no-op if absent).
    pub async fn delete_by_source(
        &self,
        workspace_id: WorkspaceId,
        source: &SourceRef,
    ) -> Result<bool> {
        let (kind, sid) = source_to_parts(source);
        let res = sqlx::query(
            "DELETE FROM documents
             WHERE workspace_id = $1 AND source_kind = $2 AND source_id = $3",
        )
        .bind(workspace_id.into_uuid())
        .bind(kind)
        .bind(sid)
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(res.rows_affected() > 0)
    }
}

/// A chunk to insert: its position, text, and (optional) Qdrant point handle.
#[derive(Clone, Debug)]
pub struct NewChunk {
    /// 0-based position within the document.
    pub ordinal: i32,
    /// The chunk text.
    pub text: String,
    /// The Qdrant point id this chunk was (or will be) upserted as.
    pub point_id: Option<Uuid>,
}

impl NewChunk {
    /// A chunk at `ordinal` holding `text`, embedded as point `point_id`.
    #[must_use]
    pub fn new(ordinal: i32, text: impl Into<String>, point_id: Option<Uuid>) -> Self {
        Self {
            ordinal,
            text: text.into(),
            point_id,
        }
    }
}

/// The full set of `chunks` columns, in row order (for `SELECT`/`RETURNING`).
const CHUNK_COLS: &str = "id, workspace_id, document_id, ordinal, text, point_id, created_at";

/// CRUD for the `chunks` table — the embedded slices of a document (SOUL §6.4).
#[derive(Clone, Debug)]
pub struct ChunkRepo {
    pool: PgPool,
}

impl ChunkRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Replace **all** of a document's chunks in one transaction: delete the old
    /// set, insert `chunks` in order. This is the re-chunk basis — a document's
    /// chunk list is regenerated wholesale on every re-ingest, so it never
    /// drifts (SOUL §3.4/§10). Returns the stored chunks in `ordinal` order.
    /// `document_id` is verified to live in `workspace_id` first, so a caller
    /// cannot write chunks across the tenancy boundary (§18).
    pub async fn replace_for_document(
        &self,
        workspace_id: WorkspaceId,
        document_id: DocumentId,
        chunks: &[NewChunk],
    ) -> Result<Vec<Chunk>> {
        let mut tx = self.pool.begin().await.map_err(map)?;

        // Tenancy guard: the document must belong to this workspace.
        let owner: Option<(Uuid,)> =
            sqlx::query_as("SELECT workspace_id FROM documents WHERE id = $1")
                .bind(document_id.into_uuid())
                .fetch_optional(&mut *tx)
                .await
                .map_err(map)?;
        match owner {
            Some((ws,)) if ws == workspace_id.into_uuid() => {}
            _ => return Err(StoreError::NotFound),
        }

        sqlx::query("DELETE FROM chunks WHERE document_id = $1")
            .bind(document_id.into_uuid())
            .execute(&mut *tx)
            .await
            .map_err(map)?;

        let mut stored = Vec::with_capacity(chunks.len());
        for c in chunks {
            let row: ChunkRow = sqlx::query_as(assert_sql_safe!(
                "INSERT INTO chunks (id, workspace_id, document_id, ordinal, text, point_id)
                 VALUES ($1, $2, $3, $4, $5, $6)
                 RETURNING {CHUNK_COLS}"
            ))
            .bind(ChunkId::new().into_uuid())
            .bind(workspace_id.into_uuid())
            .bind(document_id.into_uuid())
            .bind(c.ordinal)
            .bind(&c.text)
            .bind(c.point_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(map)?;
            stored.push(row.into());
        }

        tx.commit().await.map_err(map)?;
        Ok(stored)
    }

    /// List a document's chunks in `ordinal` order, workspace-scoped.
    pub async fn list_by_document(
        &self,
        workspace_id: WorkspaceId,
        document_id: DocumentId,
    ) -> Result<Vec<Chunk>> {
        let rows: Vec<ChunkRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {CHUNK_COLS} FROM chunks
             WHERE workspace_id = $1 AND document_id = $2
             ORDER BY ordinal ASC"
        ))
        .bind(workspace_id.into_uuid())
        .bind(document_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Chunk::from).collect())
    }

    /// Count a workspace's chunks (across all documents).
    pub async fn count_by_workspace(&self, workspace_id: WorkspaceId) -> Result<i64> {
        let (count,): (i64,) =
            sqlx::query_as("SELECT count(*) FROM chunks WHERE workspace_id = $1")
                .bind(workspace_id.into_uuid())
                .fetch_one(&self.pool)
                .await
                .map_err(map)?;
        Ok(count)
    }
}
