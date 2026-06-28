//! Object ingestion (SOUL §9/§10) — turn a stored object's *bytes* into
//! catalogued, searchable text.
//!
//! A stored object (a file in a bucket) is bytes; this pipeline extracts its text
//! into the **`documents` catalogue (Postgres truth)** and links it back from
//! `objects.extracted_text_id`, so a file becomes a first-class catalogued
//! *document* — joinable, and the substrate the embed (§6.4) + graph (§6.3)
//! layers consume. When an [`EmbedContext`] is present, the same text is also
//! chunked + embedded into Qdrant (derived) so files are **semantically
//! searchable** alongside notes and memories.
//!
//! **Text-like** objects (text/*, JSON, XML, YAML, …) are extracted directly.
//! An **image** (and, engine permitting, a PDF) is OCR'd through the optional
//! [`OcrContext`] — the `[ocr]` engine chain — into the same document pipeline;
//! with no OCR context (or an unsupported type) a binary object catalogues no
//! text, as before. PDF rasterization for the offline engines still layers on
//! later. Extraction is idempotent: a re-upload re-extracts by
//! `SourceRef::Object`, and a deleted object **purges** its document (the same
//! reconcile contract as notes, SOUL §3.1/§10).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tracing::debug;
use uuid::Uuid;

use catalerum_core::error::Error as CoreError;
use catalerum_core::id::{DocumentId, ObjectId, WorkspaceId};
use catalerum_core::model::SourceRef;
use catalerum_core::ocr::OcrRequest;
use catalerum_core::provider::{workspace_object_key, OcrEngine, StorageBackend};
use catalerum_store::{Store, StoreError};
use futures::StreamExt;
use tracing::warn;

use crate::embed::{EmbedContext, IngestReport};
use crate::error::Result;

/// The `job_queue.kind` token for an object-ingest job (SOUL §9/§10).
pub const JOB_KIND_INGEST_OBJECT: &str = "ingest_object";

/// Default cap on bytes read from an object for text extraction (16 MiB): guards
/// against loading a giant blob into memory; a larger object is truncated to this
/// (the head is the most representative for search).
const DEFAULT_MAX_BYTES: usize = 16 * 1024 * 1024;

/// The JSON payload of a [`JOB_KIND_INGEST_OBJECT`] job: which object to ingest,
/// and optionally which workspace (resolved from the job row's `workspace_id`
/// column when absent — the same shape as the other ingest payloads).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestObjectPayload {
    /// The workspace that owns the object. Optional on the wire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    /// The object to ingest.
    pub object_id: ObjectId,
}

impl IngestObjectPayload {
    /// A payload carrying an explicit workspace scope.
    #[must_use]
    pub fn new(workspace_id: WorkspaceId, object_id: ObjectId) -> Self {
        Self {
            workspace_id: Some(workspace_id),
            object_id,
        }
    }

    /// A payload that defers its scope to the job row's `workspace_id` column.
    #[must_use]
    pub fn for_object(object_id: ObjectId) -> Self {
        Self {
            workspace_id: None,
            object_id,
        }
    }
}

/// Enqueue a durable [`JOB_KIND_INGEST_OBJECT`] job for `object_id` (SOUL
/// §6.2/§10). Returns the enqueued job's id. Idempotent at the data level: each
/// run re-extracts the object's current state, so a duplicate job is at worst a
/// redundant re-projection.
pub async fn enqueue_ingest_object(
    store: &Store,
    workspace_id: WorkspaceId,
    object_id: ObjectId,
) -> Result<Uuid> {
    let payload = IngestObjectPayload::new(workspace_id, object_id);
    let job = store
        .job_queue()
        .enqueue(
            Some(workspace_id),
            JOB_KIND_INGEST_OBJECT,
            serde_json::to_value(payload)?,
            None,
        )
        .await?;
    debug!(job = %job.id, %object_id, "enqueued ingest_object job");
    Ok(job.id)
}

/// What one [`ObjectIngestContext::ingest`] run produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObjectIngestReport {
    /// The document the object's text projected to, or `None` when the object had
    /// no extractable text (binary) or was found deleted and **purged**.
    pub document_id: Option<DocumentId>,
    /// How many chunks were embedded (0 without an embed context, or when purged).
    pub chunks: usize,
    /// Bytes of extracted text (0 for a non-text / purged object).
    pub text_bytes: usize,
}

/// Whether `content_type` names a text-extractable format. Unknown / absent types
/// are **not** guessed (a missing type → not extracted), keeping extraction
/// conservative; the `+json`/`+xml` structured-suffix conventions are honored.
#[must_use]
pub fn is_text_like(content_type: Option<&str>) -> bool {
    let Some(ct) = content_type else { return false };
    let ct = ct
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    ct.starts_with("text/")
        || matches!(
            ct.as_str(),
            "application/json"
                | "application/xml"
                | "application/yaml"
                | "application/x-yaml"
                | "application/toml"
                | "application/x-toml"
                | "application/javascript"
                | "application/csv"
                // Newline-delimited JSON (data/log exports) — clearly text, and
                // reliably typed, so its lines become §10-searchable like JSON.
                | "application/x-ndjson"
                | "application/ndjson"
        )
        || ct.ends_with("+json")
        || ct.ends_with("+xml")
        || ct.ends_with("+yaml")
}

/// Extract UTF-8 text from `bytes` when `content_type` is text-like, else `None`.
/// A lossy decode tolerates stray invalid bytes in an otherwise-text file.
///
/// HTML/XHTML is first converted to **readable Markdown** (via [`catalerum_fetch`],
/// the same converter the fetch tool uses) so search indexes the document's text,
/// not its tags/scripts/styles/attributes — every other text-like type is indexed
/// verbatim, since for JSON/YAML/XML/CSV/… the structure *is* the content.
#[must_use]
pub fn extract_text(content_type: Option<&str>, bytes: &[u8]) -> Option<String> {
    if !is_text_like(content_type) {
        return None;
    }
    let raw = String::from_utf8_lossy(bytes);
    if is_html(content_type) {
        // Index the whole document (no main-content heuristic — a stored file may be
        // a fragment), dropping link URLs and images for clean, low-noise text.
        let opts = catalerum_fetch::MarkdownOptions {
            base_url: None,
            main_content_only: false,
            include_images: false,
            include_links: false,
        };
        return Some(catalerum_fetch::html_to_markdown(raw.as_ref(), &opts));
    }
    Some(raw.into_owned())
}

/// Whether `content_type`'s bare type is HTML or XHTML (so its markup should be
/// rendered to text before indexing rather than indexed verbatim).
fn is_html(content_type: Option<&str>) -> bool {
    let Some(ct) = content_type else { return false };
    let ct = ct
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    ct == "text/html" || ct == "application/xhtml+xml"
}

/// Services for OCR-ing image/PDF objects at ingest (SOUL §7/§10): the engine
/// (the core [`OcrEngine`] trait — a chain of providers in practice, so this
/// crate never names one) plus the per-kind byte caps. An oversized document is
/// **skipped, never truncated** — truncated image bytes are corrupt, unlike the
/// text path's head-truncation. Bundled separately from [`EmbedContext`] for
/// the same reason embed is: OCR layers on only when configured, and the
/// unconfigured path stays byte-identical to the pre-OCR behavior.
#[derive(Clone)]
pub struct OcrContext {
    engine: Arc<dyn OcrEngine>,
    max_image_bytes: usize,
    max_document_bytes: usize,
}

impl OcrContext {
    /// OCR through `engine` with the default caps (8 MiB images, 32 MiB PDFs).
    #[must_use]
    pub fn new(engine: Arc<dyn OcrEngine>) -> Self {
        Self {
            engine,
            max_image_bytes: 8 * 1024 * 1024,
            max_document_bytes: 32 * 1024 * 1024,
        }
    }

    /// Override the per-kind byte caps (from `[ocr]` config).
    #[must_use]
    pub fn with_limits(mut self, max_image_bytes: usize, max_document_bytes: usize) -> Self {
        self.max_image_bytes = max_image_bytes;
        self.max_document_bytes = max_document_bytes;
        self
    }

    /// The byte cap for `content_type` (PDFs get the larger document cap).
    fn max_bytes_for(&self, content_type: &str) -> usize {
        if content_type
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .eq_ignore_ascii_case("application/pdf")
        {
            self.max_document_bytes
        } else {
            self.max_image_bytes
        }
    }
}

impl std::fmt::Debug for OcrContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OcrContext")
            .field("engine", &self.engine.name())
            .field("max_image_bytes", &self.max_image_bytes)
            .field("max_document_bytes", &self.max_document_bytes)
            .finish()
    }
}

/// The services a worker needs to run an [`JOB_KIND_INGEST_OBJECT`] job: a way to
/// resolve the [`StorageBackend`] each object's bytes live on, plus a read cap.
/// An object's backend is chosen by its bucket's connection (SOUL §9): a
/// **config** backend from [`config_backends`](Self::config_backends) (keyed by
/// catalogue connection name), else a **runtime** (user-added) backend built on
/// demand from its connection's config, else the [`fallback`](Self::fallback).
/// The embed context is **not** bundled here — the worker passes its optional
/// [`EmbedContext`] through, so the Postgres-truth catalogue (document + link)
/// runs even when Qdrant is disabled, and embedding layers on only when configured
/// (SOUL §3.1: truth first, derived index optional).
#[derive(Clone)]
pub struct ObjectIngestContext {
    /// Config-defined backends by catalogue connection name (SOUL principle 10).
    config_backends: HashMap<String, Arc<dyn StorageBackend>>,
    /// Catalogue connection names of **browse** stores (keys not workspace-
    /// namespaced, SOUL §18). An object whose connection is in here is read from its
    /// raw key rather than the `<workspace_id>/` namespaced one — matching how the
    /// storage routes wrote it. Runtime stores carry the flag in their connection
    /// config and are detected per-object, so only *config* browse stores need
    /// listing here.
    browse_connections: HashSet<String>,
    /// Backend for objects whose connection can't otherwise be resolved (the
    /// legacy single-bucket catalogue, or a backend that failed to build). Usually
    /// the default store's backend.
    fallback: Option<Arc<dyn StorageBackend>>,
    max_bytes: usize,
    /// The `[ocr]` engine chain for image/PDF objects; `None` = no OCR (a
    /// binary object catalogues no text, the pre-OCR behavior).
    ocr: Option<OcrContext>,
}

impl ObjectIngestContext {
    /// Build from the config backends keyed by catalogue connection name (the
    /// [`StorageRegistry`](https://docs.rs/) map). Pair with
    /// [`with_fallback`](Self::with_fallback) for the default-store fallback.
    #[must_use]
    pub fn new(config_backends: HashMap<String, Arc<dyn StorageBackend>>) -> Self {
        Self {
            config_backends,
            browse_connections: HashSet::new(),
            fallback: None,
            max_bytes: DEFAULT_MAX_BYTES,
            ocr: None,
        }
    }

    /// A single-backend context (no per-connection map) — the simple deployment
    /// and the test path. The backend serves every object as the fallback.
    #[must_use]
    pub fn single(backend: Arc<dyn StorageBackend>) -> Self {
        Self {
            config_backends: HashMap::new(),
            browse_connections: HashSet::new(),
            fallback: Some(backend),
            max_bytes: DEFAULT_MAX_BYTES,
            ocr: None,
        }
    }

    /// Mark these catalogue connection names as **browse** stores (their keys are
    /// not workspace-namespaced, SOUL §18) — the config-defined browse backends, so
    /// the worker reads their objects from the raw key. From
    /// `StorageRegistry::browse_connections`.
    #[must_use]
    pub fn with_browse_connections(mut self, connections: HashSet<String>) -> Self {
        self.browse_connections = connections;
        self
    }

    /// Set the fallback backend used when an object's connection resolves to no
    /// registered/runtime backend.
    #[must_use]
    pub fn with_fallback(mut self, backend: Arc<dyn StorageBackend>) -> Self {
        self.fallback = Some(backend);
        self
    }

    /// Override the maximum bytes read per object for extraction.
    #[must_use]
    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    /// Attach the `[ocr]` engine chain (SOUL §7/§10): image/PDF objects the
    /// engine supports are OCR'd into the same document pipeline text-like
    /// objects use. Without this, binary objects catalogue no text.
    #[must_use]
    pub fn with_ocr(mut self, ocr: OcrContext) -> Self {
        self.ocr = Some(ocr);
        self
    }

    /// Resolve the backend an object's bytes live on (SOUL §9): config backend by
    /// its bucket's connection name, else a runtime backend built from the
    /// connection's config, else the fallback. Returns the backend **and whether
    /// its keys are workspace-namespaced** (`false` for a browse store, SOUL §18) so
    /// [`read_text`](Self::read_text) reads from the right physical key.
    async fn resolve_backend(
        &self,
        store: &Store,
        workspace_id: WorkspaceId,
        object: &catalerum_core::model::StoredObject,
    ) -> Result<(Arc<dyn StorageBackend>, bool)> {
        let bucket = store.buckets().get(workspace_id, object.bucket_id).await?;
        let connection = store
            .connections()
            .get(workspace_id, bucket.connection_id)
            .await?;
        if let Some(backend) = self.config_backends.get(&connection.name) {
            let namespaced = !self.browse_connections.contains(&connection.name);
            return Ok((backend.clone(), namespaced));
        }
        // A runtime (user-added) backend: build it from the connection's config —
        // which also carries its `browse` flag.
        let row = store
            .connections()
            .get_row(workspace_id, bucket.connection_id)
            .await?;
        if let Ok(backend) = catalerum_storage::backend_from_connection(&connection, row.config()) {
            let browse = row
                .config()
                .get("browse")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            return Ok((backend, !browse));
        }
        // Fallback is always the namespaced default store.
        self.fallback.clone().map(|b| (b, true)).ok_or_else(|| {
            catalerum_core::error::Error::invalid(format!(
                "no storage backend for object {} (connection `{}`)",
                object.id, connection.name
            ))
            .into()
        })
    }

    /// Ingest one object (SOUL §9/§10). Reconciles to the object's *current*
    /// state: a present text object (re-)extracts → upserts its `documents` row
    /// and links `extracted_text_id` (Postgres truth), and — when `embed` is
    /// `Some` — embeds its chunks into Qdrant (derived). A non-text object clears
    /// any stale projection. An object found **deleted** purges its document (and
    /// vectors), so a delete reconciles like any edit.
    pub async fn ingest(
        &self,
        store: &Store,
        embed: Option<&EmbedContext>,
        workspace_id: WorkspaceId,
        object_id: ObjectId,
    ) -> Result<ObjectIngestReport> {
        let source = SourceRef::Object { id: object_id };

        // Reconcile: a deleted object purges its derived projection (§3.1/§10).
        let object = match store.objects().get(workspace_id, object_id).await {
            Ok(o) => o,
            Err(StoreError::NotFound) => {
                match embed {
                    Some(e) => {
                        e.purge(store, workspace_id, &source).await?;
                    }
                    None => {
                        store
                            .documents()
                            .delete_by_source(workspace_id, &source)
                            .await?;
                    }
                }
                debug!(%object_id, "object deleted; purged its document projection");
                return Ok(ObjectIngestReport {
                    document_id: None,
                    chunks: 0,
                    text_bytes: 0,
                });
            }
            Err(e) => return Err(e.into()),
        };

        // A non-text object catalogues no text: purge any stale projection so a
        // text→binary overwrite (same key, same object id) leaves no orphaned
        // document AND no orphaned Qdrant vectors. This must reconcile identically
        // to a delete — purge_source clears vectors *then* the document — so when
        // an embed context is present we route through it; Postgres-only otherwise
        // (SOUL §3.1/§10). Clearing the link last is belt-and-suspenders (the FK
        // is `ON DELETE SET NULL`, but the document delete already nulled it).
        // Resolve the backend the object's bytes live on (only for extractable
        // objects — a plain binary skips the read entirely, so we skip the
        // lookup too). Text-like types decode directly; an image/PDF the OCR
        // chain supports is OCR'd into the same pipeline (SOUL §7/§10).
        let content_type = object.content_type.as_deref();
        let text = if is_text_like(content_type) {
            let (backend, namespaced) = self.resolve_backend(store, workspace_id, &object).await?;
            self.read_text(
                backend.as_ref(),
                namespaced,
                workspace_id,
                &object.key,
                content_type,
            )
            .await?
        } else if let Some(ocr) = self
            .ocr
            .as_ref()
            .filter(|o| content_type.is_some_and(|ct| o.engine.supports(ct)))
        {
            let ct = content_type.unwrap_or_default();
            let max = ocr.max_bytes_for(ct);
            if object.size > max as u64 {
                // Skip, never truncate: truncated image/PDF bytes are corrupt.
                debug!(%object_id, size = object.size, max, "object over the OCR byte cap; cataloguing no text");
                None
            } else {
                let (backend, namespaced) =
                    self.resolve_backend(store, workspace_id, &object).await?;
                // Read one byte past the cap so a stale `size` can't silently
                // hand the engine a truncated document.
                let bytes = self
                    .read_bytes(
                        backend.as_ref(),
                        namespaced,
                        workspace_id,
                        &object.key,
                        max + 1,
                    )
                    .await?;
                if bytes.len() > max {
                    debug!(%object_id, read = bytes.len(), max, "object outgrew its catalogued size past the OCR cap; cataloguing no text");
                    None
                } else {
                    match ocr.engine.ocr(OcrRequest::new(bytes, ct)).await {
                        // A text-free image catalogues no document (the purge
                        // path below reconciles) — cleaner than empty documents.
                        Ok(resp) if resp.text.trim().is_empty() => None,
                        Ok(resp) => Some(resp.text),
                        // A permanent rejection (model can't take images, bad
                        // key, undecodable bytes) must not burn the job's
                        // retries on every scan: warn + catalogue no text.
                        Err(CoreError::Unsupported(msg)) | Err(CoreError::Invalid(msg)) => {
                            warn!(%object_id, content_type = %ct, error = %msg,
                                "OCR permanently rejected the object; cataloguing no text");
                            None
                        }
                        // Transient trouble (provider outage, timeout) fails
                        // the job → worker retry/backoff, same as embed errors.
                        Err(e) => return Err(e.into()),
                    }
                }
            }
        } else {
            None
        };
        let Some(text) = text else {
            match embed {
                Some(e) => {
                    e.purge(store, workspace_id, &source).await?;
                }
                None => {
                    store
                        .documents()
                        .delete_by_source(workspace_id, &source)
                        .await?;
                }
            }
            store
                .objects()
                .set_extracted_text(workspace_id, object_id, None)
                .await?;
            debug!(%object_id, content_type = ?object.content_type, "object not text-extractable; purged any prior projection");
            return Ok(ObjectIngestReport {
                document_id: None,
                chunks: 0,
                text_bytes: 0,
            });
        };
        let text_bytes = text.len();

        // Truth first: catalogue the document. With an embed context the same
        // upsert runs inside the derived pipeline (idempotent), so we don't
        // double-write — pick one path.
        let report = match embed {
            Some(e) => {
                // Denormalize the object's (bucket name, key) into each vector so
                // semantic search can scope to a bucket / subdir prefix. The bucket
                // name matches the catalogue label and the `StorageObject` trigger's
                // `bucket` (both from the bucket row), keeping ingest, search, and
                // the Phase-C de-index path on one identifier.
                let bucket_name = store
                    .buckets()
                    .get(workspace_id, object.bucket_id)
                    .await
                    .map(|b| b.name)
                    .unwrap_or_default();
                e.ingest_text(
                    store,
                    workspace_id,
                    &source,
                    &text,
                    Some((&bucket_name, &object.key)),
                    object.last_modified,
                )
                .await?
            }
            None => {
                let doc = store
                    .documents()
                    .upsert_by_source(workspace_id, &source, &text, None)
                    .await?;
                IngestReport {
                    document_id: Some(doc.id),
                    chunks: 0,
                }
            }
        };

        // Link the object → its extracted-text document (§10).
        if let Some(doc_id) = report.document_id {
            store
                .objects()
                .set_extracted_text(workspace_id, object_id, Some(doc_id))
                .await?;
        }
        debug!(%object_id, document = ?report.document_id, chunks = report.chunks, "ingest_object done");
        Ok(ObjectIngestReport {
            document_id: report.document_id,
            chunks: report.chunks,
            text_bytes,
        })
    }

    /// Read up to `max_bytes` of the object and extract its text (text-like types
    /// only). Returns `None` for a binary object (read is skipped entirely). The
    /// blob is read from the **workspace-namespaced** physical key (SOUL §18), the
    /// same convention the storage routes write under — except on a browse store
    /// (`namespaced == false`), where the raw key is read (its files as they sit on
    /// disk).
    async fn read_text(
        &self,
        backend: &dyn StorageBackend,
        namespaced: bool,
        workspace_id: WorkspaceId,
        key: &str,
        content_type: Option<&str>,
    ) -> Result<Option<String>> {
        if !is_text_like(content_type) {
            return Ok(None);
        }
        let buf = self
            .read_bytes(backend, namespaced, workspace_id, key, self.max_bytes)
            .await?;
        Ok(extract_text(content_type, &buf))
    }

    /// Read up to `cap` bytes of the object from its **workspace-namespaced**
    /// physical key (raw key on a browse store) — the shared byte loop under
    /// [`read_text`](Self::read_text) (head-truncating) and the OCR path (which
    /// sizes `cap` to detect, not truncate).
    async fn read_bytes(
        &self,
        backend: &dyn StorageBackend,
        namespaced: bool,
        workspace_id: WorkspaceId,
        key: &str,
        cap: usize,
    ) -> Result<Vec<u8>> {
        let physical = if namespaced {
            workspace_object_key(workspace_id, key)
        } else {
            key.trim_start_matches('/').to_string()
        };
        let mut stream = backend.get(&physical).await?;
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            let room = cap.saturating_sub(buf.len());
            if room == 0 {
                break;
            }
            if chunk.len() > room {
                buf.extend_from_slice(&chunk[..room]);
                break;
            }
            buf.extend_from_slice(&chunk);
        }
        Ok(buf)
    }
}

impl std::fmt::Debug for ObjectIngestContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObjectIngestContext")
            .field("max_bytes", &self.max_bytes)
            .field("has_ocr", &self.ocr.is_some())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_like_classification() {
        assert!(is_text_like(Some("text/markdown")));
        assert!(is_text_like(Some("text/plain; charset=utf-8")));
        assert!(is_text_like(Some("application/json")));
        assert!(is_text_like(Some("application/vnd.api+json")));
        assert!(is_text_like(Some("APPLICATION/XML")));
        // NDJSON (data/log exports) is recognized, including with a charset param.
        assert!(is_text_like(Some("application/x-ndjson")));
        assert!(is_text_like(Some("application/ndjson; charset=utf-8")));
        assert!(!is_text_like(Some("image/png")));
        assert!(!is_text_like(Some("application/octet-stream")));
        assert!(!is_text_like(None));
    }

    #[test]
    fn extract_text_decodes_text_skips_binary() {
        assert_eq!(
            extract_text(Some("text/plain"), b"hello world"),
            Some("hello world".to_string())
        );
        // Lossy decode tolerates a stray invalid byte.
        assert_eq!(
            extract_text(Some("text/plain"), &[0x68, 0x69, 0xff]),
            Some("hi\u{fffd}".to_string())
        );
        assert_eq!(extract_text(Some("image/png"), &[0x89, 0x50]), None);
        assert_eq!(extract_text(None, b"data"), None);
    }

    #[test]
    fn extract_text_renders_html_to_readable_text() {
        // HTML is indexed as its readable text, not raw markup: tags, scripts and
        // styles are gone; the heading and paragraph text remain.
        let html = b"<html><head><style>.x{color:red}</style></head>\
                     <body><h1>Title</h1><p>Hello <b>world</b>.</p>\
                     <script>var secret=1;</script></body></html>";
        let out = extract_text(Some("text/html"), html).unwrap();
        assert!(out.contains("Title"), "{out:?}");
        assert!(out.contains("Hello") && out.contains("world"), "{out:?}");
        assert!(
            !out.contains("<h1>") && !out.contains("<p>"),
            "tags leaked: {out:?}"
        );
        assert!(
            !out.contains("color:red") && !out.contains("secret"),
            "css/js leaked: {out:?}"
        );
        // A charset param on the content type is tolerated.
        let out2 = extract_text(Some("text/html; charset=utf-8"), b"<p>plain</p>").unwrap();
        assert_eq!(out2, "plain");
        // A non-HTML text type is still indexed verbatim (structure is the content).
        assert_eq!(
            extract_text(Some("application/xml"), b"<note>hi</note>"),
            Some("<note>hi</note>".to_string())
        );
    }

    #[test]
    fn payload_round_trips_and_accepts_object_only_shape() {
        let p = IngestObjectPayload::new(WorkspaceId::new(), ObjectId::new());
        let json = serde_json::to_value(p).unwrap();
        assert!(json.get("workspace_id").is_some());
        assert!(json.get("object_id").is_some());
        let back: IngestObjectPayload = serde_json::from_value(json).unwrap();
        assert_eq!(p, back);

        let oid = ObjectId::new();
        let only = serde_json::json!({ "object_id": oid });
        let p2: IngestObjectPayload = serde_json::from_value(only).unwrap();
        assert_eq!(p2.workspace_id, None);
        assert_eq!(p2.object_id, oid);
        assert_eq!(IngestObjectPayload::for_object(oid), p2);
    }

    #[test]
    fn job_kind_token_is_stable() {
        assert_eq!(JOB_KIND_INGEST_OBJECT, "ingest_object");
    }
}
