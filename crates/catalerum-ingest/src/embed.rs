//! The embed→upsert ingest pipeline (SOUL §6.4/§10): turn a note's text into
//! Postgres `documents`/`chunks` and a derived Qdrant vector set.
//!
//! [`ingest_note`] is the unit of work: load the note (Postgres truth), upsert
//! its [`Document`](catalerum_core::Document), chunk the text ([`crate::chunk`]),
//! embed each chunk through the llmleaf [`Embedder`] (no concrete provider — any
//! `Embedder`), persist the chunks, and upsert one Qdrant point per chunk. It is
//! **idempotent** (SOUL §3.4): the document id is stable across re-ingests, the
//! chunk set is replaced wholesale, and the prior Qdrant points for the source
//! are deleted before the new ones land — so re-ingesting an edited note leaves
//! no orphan vectors and never duplicates.
//!
//! The whole thing is **derived and rebuildable** (principle 1, §3.1): drop the
//! `chunks` rows and the Qdrant collection, re-run `ingest_note`, and the index
//! reprojects from the note row with no data loss.
//!
//! # Job contract
//! [`enqueue_ingest_note`] writes a durable [`JOB_KIND_INGEST_NOTE`] job whose
//! payload is [`IngestNotePayload`]. A worker that holds an [`Embedder`] + a
//! [`VectorStore`] runs [`ingest_note`] for it. (Wiring this kind into the
//! polling worker + the binary is the follow-on step; the contract is fixed
//! here so the producer side — e.g. the note-write API — can enqueue today.)

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::debug;
use uuid::Uuid;

use catalerum_core::embed::EmbeddingRequest;
use catalerum_core::id::{DocumentId, MemoryId};
use catalerum_core::provider::Embedder;
use catalerum_core::{NoteId, SourceRef, WorkspaceId};
use catalerum_store::{NewChunk, Store};
use catalerum_vector::{PointPayload, VectorPoint, VectorStore};

use crate::chunk::{chunk_text, ChunkConfig};
use crate::error::{IngestError, Result};

/// The `job_queue.kind` token for a note-embed job.
pub const JOB_KIND_INGEST_NOTE: &str = "ingest_note";

/// The `job_queue.kind` token for a memory-embed job (SOUL §22).
pub const JOB_KIND_INGEST_MEMORY: &str = "ingest_memory";

/// Tuning for one ingest run: which embedding model to call and how to chunk.
///
/// The collection's vector **width is discovered from the embedder**, not
/// configured: the model is the source of truth for its own dimensionality, so
/// there is no width to keep in sync. `ensure_collection` still refuses a later
/// width change (a model swap without a rebuild) — that surfaces as an error
/// rather than silent corruption (SOUL §3.1).
#[derive(Clone, Debug)]
pub struct IngestConfig {
    /// The llmleaf embedding model (an `[llm]` config field, SOUL §7/§13).
    pub embed_model: String,
    /// How to split the note text into chunks.
    pub chunk: ChunkConfig,
}

impl IngestConfig {
    /// A config for `embed_model` with the default chunking.
    #[must_use]
    pub fn new(embed_model: impl Into<String>) -> Self {
        Self {
            embed_model: embed_model.into(),
            chunk: ChunkConfig::default(),
        }
    }

    /// Override the chunking strategy.
    #[must_use]
    pub fn with_chunk(mut self, chunk: ChunkConfig) -> Self {
        self.chunk = chunk;
        self
    }
}

/// What one [`ingest_note`] run produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IngestReport {
    /// The (stable) document the note projected to, or `None` when the note was
    /// found deleted and its projection was **purged** instead.
    pub document_id: Option<DocumentId>,
    /// How many chunks were embedded + upserted (0 for an empty or purged note).
    pub chunks: usize,
}

/// Ingest one note into Postgres `documents`/`chunks` and the derived Qdrant
/// index (SOUL §6.4/§10). Idempotent — safe to re-run on every note edit.
///
/// Steps: load note → upsert document → chunk → embed → ensure collection at the
/// embedder's width → clear prior vectors for the source → persist chunks
/// (truth) → upsert vectors (derived). `ensure_collection` runs **before** the
/// delete, so a width change (a swapped embedding model) is refused before any
/// vectors are destroyed.
pub async fn ingest_note<E: Embedder + ?Sized>(
    store: &Store,
    embedder: &E,
    vector: &VectorStore,
    cfg: &IngestConfig,
    workspace_id: WorkspaceId,
    note_id: NoteId,
) -> Result<IngestReport> {
    let source = SourceRef::Note { id: note_id };
    // Reconcile to the note's *current* Postgres state: a present note (re-)embeds;
    // a note found deleted is **purged**, so a delete reconciles like any edit
    // (SOUL §3.1/§10).
    let note = match store.notes().get(workspace_id, note_id).await {
        Ok(n) => n,
        Err(catalerum_store::StoreError::NotFound) => {
            return purge_source(store, vector, workspace_id, &source).await
        }
        Err(e) => return Err(e.into()),
    };
    // Embeddable text: the title gives the body context, especially for short
    // notes whose body alone is ambiguous.
    let text = if note.markdown.trim().is_empty() {
        note.title.clone()
    } else {
        format!("{}\n\n{}", note.title, note.markdown)
    };
    ingest_source(
        store,
        embedder,
        vector,
        cfg,
        workspace_id,
        &source,
        &text,
        None,
        note.updated_at,
    )
    .await
}

/// Ingest one memory into the derived Qdrant index (SOUL §6.4/§22): load the
/// memory (Postgres truth), embed its text, upsert one Qdrant point per chunk
/// (`SourceRef::Memory`); a memory found deleted is **purged**. Reuses the same
/// reconcile/idempotency core as [`ingest_note`].
///
/// **Visibility note:** user-scoped (private) memories *are* embedded, so the
/// retrieval surface (`search_semantic`) must re-check each memory hit's
/// visibility against Postgres before returning it — embedding alone does not
/// encode who may see it (enforced at the tool layer, SOUL §18/§22).
pub async fn ingest_memory<E: Embedder + ?Sized>(
    store: &Store,
    embedder: &E,
    vector: &VectorStore,
    cfg: &IngestConfig,
    workspace_id: WorkspaceId,
    memory_id: MemoryId,
) -> Result<IngestReport> {
    let source = SourceRef::Memory { id: memory_id };
    let memory = match store.memories().get(workspace_id, memory_id).await {
        Ok(m) => m,
        Err(catalerum_store::StoreError::NotFound) => {
            return purge_source(store, vector, workspace_id, &source).await
        }
        Err(e) => return Err(e.into()),
    };
    ingest_source(
        store,
        embedder,
        vector,
        cfg,
        workspace_id,
        &source,
        &memory.text,
        None,
        memory.created_at,
    )
    .await
}

/// Embed (or re-embed) one source's `text` into Postgres `documents`/`chunks` and
/// the derived Qdrant index — the idempotent reconcile core shared by notes and
/// memories (SOUL §6.4/§10): upsert document → chunk → embed → ensure collection
/// at the embedder's width → clear prior vectors → persist chunks (truth) →
/// upsert vectors (derived). `ensure_collection` runs **before** the delete, so a
/// width change is refused before any vectors are destroyed.
#[allow(clippy::too_many_arguments)] // an internal pipeline step; bundling would obscure it
pub(crate) async fn ingest_source<E: Embedder + ?Sized>(
    store: &Store,
    embedder: &E,
    vector: &VectorStore,
    cfg: &IngestConfig,
    workspace_id: WorkspaceId,
    source: &SourceRef,
    text: &str,
    storage: Option<(&str, &str)>,
    created_at: DateTime<Utc>,
) -> Result<IngestReport> {
    let document = store
        .documents()
        .upsert_by_source(workspace_id, source, text, None)
        .await?;

    let chunk_texts = chunk_text(text, &cfg.chunk);

    if chunk_texts.is_empty() {
        // An empty source owns an empty chunk set: clear both stores. The vector
        // delete is lenient on a not-yet-created collection (SOUL §3.4).
        vector.delete_by_source(workspace_id, source).await?;
        store
            .chunks()
            .replace_for_document(workspace_id, document.id, &[])
            .await?;
        debug!(document = %document.id, "ingested empty source (0 chunks)");
        return Ok(IngestReport {
            document_id: Some(document.id),
            chunks: 0,
        });
    }

    let resp = embedder
        .embed(EmbeddingRequest::new(
            cfg.embed_model.clone(),
            chunk_texts.clone(),
        ))
        .await?;
    if resp.embeddings.len() != chunk_texts.len() {
        return Err(IngestError::Embed(format!(
            "embedder returned {} vectors for {} chunks",
            resp.embeddings.len(),
            chunk_texts.len()
        )));
    }
    let dim = resp
        .dimensions()
        .ok_or_else(|| IngestError::Embed("embedder returned no vectors".into()))?
        as u64;

    vector.ensure_collection(workspace_id, dim).await?;
    vector.delete_by_source(workspace_id, source).await?;

    let mut points = Vec::with_capacity(chunk_texts.len());
    let mut new_chunks = Vec::with_capacity(chunk_texts.len());
    for (ordinal, (chunk, emb)) in chunk_texts.iter().zip(resp.embeddings.iter()).enumerate() {
        if emb.vector.len() as u64 != dim {
            return Err(IngestError::Embed(format!(
                "embedder returned inconsistent widths ({} vs {dim})",
                emb.vector.len()
            )));
        }
        let point_id = Uuid::new_v4();
        let mut payload = PointPayload::new(workspace_id, source.clone(), chunk.clone())
            .with_created_at(created_at);
        if let Some((bucket, key)) = storage {
            payload = payload.with_storage(bucket, key);
        }
        points.push(VectorPoint::with_id(point_id, emb.vector.clone(), payload));
        new_chunks.push(NewChunk::new(ordinal as i32, chunk.clone(), Some(point_id)));
    }

    // Postgres truth first, then the derived vector upsert (principle 1/7).
    store
        .chunks()
        .replace_for_document(workspace_id, document.id, &new_chunks)
        .await?;
    vector.upsert(workspace_id, &points).await?;

    debug!(document = %document.id, chunks = new_chunks.len(), "ingested source");
    Ok(IngestReport {
        document_id: Some(document.id),
        chunks: new_chunks.len(),
    })
}

/// Purge a source's derived projection — clear its Qdrant vectors and its
/// `documents` row (cascading to `chunks`). The reconcile path when the source
/// row is gone (SOUL §3.1/§10); idempotent and lenient on a missing collection.
pub(crate) async fn purge_source(
    store: &Store,
    vector: &VectorStore,
    workspace_id: WorkspaceId,
    source: &SourceRef,
) -> Result<IngestReport> {
    vector.delete_by_source(workspace_id, source).await?;
    store
        .documents()
        .delete_by_source(workspace_id, source)
        .await?;
    debug!(?source, "purged deleted source projection");
    Ok(IngestReport {
        document_id: None,
        chunks: 0,
    })
}

/// The services a worker needs to run an [`ingest_note`] job: an [`Embedder`]
/// (llmleaf or any), the derived [`VectorStore`], and the [`IngestConfig`].
/// Bundled so the polling worker holds one optional handle and dispatches
/// `ingest_note` jobs when present (SOUL §10). Cloning is cheap (the embedder is
/// `Arc`-shared and the vector store is a thin client).
#[derive(Clone)]
pub struct EmbedContext {
    /// The embedding client.
    pub embedder: Arc<dyn Embedder>,
    /// The derived Qdrant vector index.
    pub vector: VectorStore,
    /// Embedding-model + chunking tuning.
    pub config: IngestConfig,
}

impl EmbedContext {
    /// Bundle the services for note ingestion.
    #[must_use]
    pub fn new(embedder: Arc<dyn Embedder>, vector: VectorStore, config: IngestConfig) -> Self {
        Self {
            embedder,
            vector,
            config,
        }
    }

    /// Run [`ingest_note`] for `note_id` using these services.
    pub async fn ingest_note(
        &self,
        store: &Store,
        workspace_id: WorkspaceId,
        note_id: NoteId,
    ) -> Result<IngestReport> {
        ingest_note(
            store,
            &*self.embedder,
            &self.vector,
            &self.config,
            workspace_id,
            note_id,
        )
        .await
    }

    /// Run [`ingest_memory`] for `memory_id` using these services.
    pub async fn ingest_memory(
        &self,
        store: &Store,
        workspace_id: WorkspaceId,
        memory_id: MemoryId,
    ) -> Result<IngestReport> {
        ingest_memory(
            store,
            &*self.embedder,
            &self.vector,
            &self.config,
            workspace_id,
            memory_id,
        )
        .await
    }

    /// Embed pre-extracted `text` for an arbitrary `source` (e.g. an object's
    /// extracted text, SOUL §10) into `documents`/`chunks` + Qdrant. The
    /// idempotent reconcile core shared with notes/memories.
    ///
    /// `storage` denormalizes the source object's `(bucket, key)` into each
    /// vector so search can scope to a bucket / subdir prefix; pass `None` for
    /// sources with no storage path (notes, memories, emails).
    pub async fn ingest_text(
        &self,
        store: &Store,
        workspace_id: WorkspaceId,
        source: &SourceRef,
        text: &str,
        storage: Option<(&str, &str)>,
        created_at: DateTime<Utc>,
    ) -> Result<IngestReport> {
        ingest_source(
            store,
            &*self.embedder,
            &self.vector,
            &self.config,
            workspace_id,
            source,
            text,
            storage,
            created_at,
        )
        .await
    }

    /// Purge a source's derived projection (Qdrant vectors + `documents` row) —
    /// the reconcile path when the source is gone (SOUL §3.1/§10).
    pub async fn purge(
        &self,
        store: &Store,
        workspace_id: WorkspaceId,
        source: &SourceRef,
    ) -> Result<IngestReport> {
        purge_source(store, &self.vector, workspace_id, source).await
    }
}

impl std::fmt::Debug for EmbedContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The embedder is an opaque trait object; show only the config.
        f.debug_struct("EmbedContext")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// The JSON payload of a [`JOB_KIND_INGEST_NOTE`] job: which note to ingest, and
/// optionally which workspace (resolved from the job row's `workspace_id` column
/// when absent — the same shape as [`crate::SyncCalendarPayload`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestNotePayload {
    /// The workspace that owns the note. Optional on the wire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    /// The note to ingest.
    pub note_id: NoteId,
}

impl IngestNotePayload {
    /// A payload carrying an explicit workspace scope.
    #[must_use]
    pub fn new(workspace_id: WorkspaceId, note_id: NoteId) -> Self {
        Self {
            workspace_id: Some(workspace_id),
            note_id,
        }
    }

    /// A payload that defers its scope to the job row's `workspace_id` column.
    #[must_use]
    pub fn for_note(note_id: NoteId) -> Self {
        Self {
            workspace_id: None,
            note_id,
        }
    }
}

/// Enqueue a durable [`JOB_KIND_INGEST_NOTE`] job for `note_id` (SOUL §6.2/§10).
/// Returns the enqueued job's id. Idempotent at the data level: each run is an
/// idempotent ingest, so a duplicate job is at worst a redundant re-projection.
pub async fn enqueue_ingest_note(
    store: &Store,
    workspace_id: WorkspaceId,
    note_id: NoteId,
) -> Result<Uuid> {
    let payload = IngestNotePayload::new(workspace_id, note_id);
    let job = store
        .job_queue()
        .enqueue(
            Some(workspace_id),
            JOB_KIND_INGEST_NOTE,
            serde_json::to_value(payload)?,
            None,
        )
        .await?;
    debug!(job = %job.id, %note_id, "enqueued ingest_note job");
    Ok(job.id)
}

/// The JSON payload of a [`JOB_KIND_INGEST_MEMORY`] job (same workspace-optional
/// shape as [`IngestNotePayload`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestMemoryPayload {
    /// The workspace that owns the memory. Optional on the wire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    /// The memory to ingest.
    pub memory_id: MemoryId,
}

impl IngestMemoryPayload {
    /// A payload carrying an explicit workspace scope.
    #[must_use]
    pub fn new(workspace_id: WorkspaceId, memory_id: MemoryId) -> Self {
        Self {
            workspace_id: Some(workspace_id),
            memory_id,
        }
    }

    /// A payload that defers its scope to the job row's `workspace_id` column.
    #[must_use]
    pub fn for_memory(memory_id: MemoryId) -> Self {
        Self {
            workspace_id: None,
            memory_id,
        }
    }
}

/// Enqueue a durable [`JOB_KIND_INGEST_MEMORY`] job for `memory_id` (SOUL §22).
pub async fn enqueue_ingest_memory(
    store: &Store,
    workspace_id: WorkspaceId,
    memory_id: MemoryId,
) -> Result<Uuid> {
    let payload = IngestMemoryPayload::new(workspace_id, memory_id);
    let job = store
        .job_queue()
        .enqueue(
            Some(workspace_id),
            JOB_KIND_INGEST_MEMORY,
            serde_json::to_value(payload)?,
            None,
        )
        .await?;
    debug!(job = %job.id, %memory_id, "enqueued ingest_memory job");
    Ok(job.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_round_trips_and_accepts_note_only_shape() {
        let p = IngestNotePayload::new(WorkspaceId::new(), NoteId::new());
        let json = serde_json::to_value(p).unwrap();
        assert!(json.get("workspace_id").is_some());
        assert!(json.get("note_id").is_some());
        assert_eq!(
            serde_json::from_value::<IngestNotePayload>(json).unwrap(),
            p
        );

        // The note-only shape (scope from the job row) must deserialize.
        let note = NoteId::new();
        let p2: IngestNotePayload =
            serde_json::from_value(serde_json::json!({ "note_id": note })).unwrap();
        assert_eq!(p2.workspace_id, None);
        assert_eq!(p2.note_id, note);
        // And the constructor for it omits the workspace key.
        let reser = serde_json::to_value(IngestNotePayload::for_note(note)).unwrap();
        assert!(reser.get("workspace_id").is_none());
    }

    #[test]
    fn job_kind_token_is_stable() {
        assert_eq!(JOB_KIND_INGEST_NOTE, "ingest_note");
    }

    #[test]
    fn config_defaults_and_builder() {
        let cfg = IngestConfig::new("text-embedding-3-small");
        assert_eq!(cfg.embed_model, "text-embedding-3-small");
        // Default chunking until overridden.
        assert_eq!(cfg.chunk.max_chars, ChunkConfig::default().max_chars);
        let cfg = cfg.with_chunk(ChunkConfig::sized(50));
        assert_eq!(cfg.chunk.max_chars, 50);
    }
}
