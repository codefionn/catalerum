//! Object storage REST surface (SOUL §9/§12) over the workspace's storage backends
//! ("stores"). A workspace can hold **many** backends — the config-defined ones
//! (`[storage]` + `[storage.backends.*]`) plus runtime (user-added) ones — so a
//! file chooses where it lives via `?store=` (omitted → the default store). Each is
//! resolved per-request by [`resolve`] to a [`StorageHandle`](crate::state::StorageHandle).
//!
//! **Blob layer** (the filesystem/S3/WebDAV backend; `404` when no store resolves):
//! - `GET    /storage/objects?prefix=…&store=…` list backend object metadata (`storage:read`)
//! - `PUT    /storage/objects/{*key}?store=…`    upload an object (`storage:write`)
//! - `GET    /storage/objects/{*key}?store=…`    download bytes (`storage:read`)
//! - `DELETE /storage/objects/{*key}?store=…`    remove an object (`storage:write`)
//!
//! **Stores** (the selectable backends, config + runtime):
//! - `GET    /storage/stores`               list backends (`storage:read`; no secrets)
//! - `POST   /storage/stores`               add a runtime backend (`storage:write`)
//! - `DELETE /storage/stores/{name}`        remove a runtime backend (`storage:write`)
//!
//! **Catalogue layer** (Postgres truth — buckets/objects rows, works regardless of
//! whether a backend is currently configured):
//! - `GET    /storage/buckets`              list catalogued buckets (`storage:read`)
//! - `GET    /storage/catalogue?prefix=…`   list catalogued objects, each with its
//!   bucket name, store, + §10 extracted-text link (`storage:read`)
//!
//! **Labels layer** (user/agent tags on files & directories — keyed by
//! `(store, path)`, so a directory (no object row) can be tagged too):
//! - `GET    /storage/labels?store=&prefix=` list a store's labels (or `?label=` to
//!   list every path with a label) (`storage:read`)
//! - `GET    /storage/labels/for?store=&path=` labels on one path (`storage:read`)
//! - `POST   /storage/labels`               apply a label (`storage:write`)
//! - `DELETE /storage/labels/{id}`          remove a label (`storage:write`)
//!
//! An upload also fires the **`StorageObject` automation trigger** (§11) — the
//! event source that was previously inert — so "when a file lands, run X" works,
//! the storage analogue of the webhook §25 / Kanban §24 sources. Every route is
//! authenticated + workspace-scoped + capability-gated (principle 15). Blobs are
//! physically namespaced per workspace (`<workspace_id>/<key>`, §18); the
//! catalogue + API surface keep user-facing keys.

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};

use std::collections::HashMap;

use catalerum_core::capability::Action;
use catalerum_core::model::{Author, Bucket, ConnectionKind, ObjectLabel};
use catalerum_core::preview::{PreviewFormat, PreviewRequest};
use catalerum_core::provider::{ObjectMeta, PutMeta};
use catalerum_core::{
    BucketId, DocumentId, Error as CoreError, ObjectId, ObjectLabelId, SourceRef, WorkspaceId,
};
use catalerum_store::{
    StoreError, DEFAULT_LABEL_LIMIT, DEFAULT_OBJECT_LIMIT, DEFAULT_OBJECT_SEARCH_LIMIT,
};

use crate::auth::Auth;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

use crate::object_service::ScanReport;
use crate::object_service::{
    delete_object_at, dispatch_storage_event, read_object_bytes, resolve,
    runtime_watch, scan_store, validate_object_key, ScanEvents,
};
use crate::object_service::{
    catalogue_object, find_storage_connection, map_storage_err, runtime_bucket_name,
    runtime_stores, visibility_workspace,
};

/// Mount the storage routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/storage/objects", get(list))
        .route(
            "/storage/objects/{*key}",
            put(upload).get(download).delete(remove),
        )
        // Image previews (§9/§10): render an ad-hoc uploaded document, or a
        // stored object by key, to a fitted image. `POST` carries its own body
        // (capped); `GET` reads the stored object. A wildcard `{*key}` must be
        // the last path segment, so preview lives under its own prefix rather
        // than as an `/objects/{key}/preview` suffix.
        .route(
            "/storage/preview",
            post(preview_upload).layer(DefaultBodyLimit::max(MAX_PREVIEW_BYTES)),
        )
        .route("/storage/preview/{*key}", get(preview_object))
        .route("/storage/buckets", get(list_buckets))
        .route("/storage/catalogue", get(list_catalogue))
        .route("/storage/catalogue/search", get(search_objects))
        .route("/storage/catalogue/{id}/text", get(object_text))
        // Labels on files & directories (§9). `/labels/for` before `/labels/{id}`
        // so the literal path wins over the `{id}` capture.
        .route("/storage/labels", get(list_labels).post(add_label))
        .route("/storage/labels/for", get(list_labels_for))
        .route("/storage/labels/{id}", axum::routing::delete(delete_label))
        .route("/storage/stores", get(list_stores).post(create_store))
        .route(
            "/storage/stores/{name}",
            axum::routing::delete(delete_store),
        )
        .route(
            "/storage/stores/{name}/scan",
            axum::routing::post(scan_route),
        )
}

/// Query for `GET /storage/catalogue/search`: `q` is the substring to find in
/// objects' §10 extracted text, `limit` caps results.
#[derive(Debug, Deserialize)]
struct ObjectSearchQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    limit: Option<u32>,
}

/// One object content-search hit: which object matched + a short excerpt of its
/// extracted text windowed around the match.
#[derive(Debug, Serialize)]
struct ObjectTextHitView {
    id: ObjectId,
    key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content_type: Option<String>,
    excerpt: String,
    /// The `?store=` selector this hit lives on, so a download targets the right
    /// backend (content search spans every store, SOUL §9). Empty if unresolved.
    store: String,
}

/// `GET /storage/catalogue/search?q=&limit=` — search objects by the **content**
/// of their §10 extracted text (`storage:read`, workspace-scoped), newest-modified
/// first, each hit carrying a match-windowed excerpt. Only ingested objects can
/// match; a blank `q` returns `[]`.
async fn search_objects(
    State(state): State<AppState>,
    auth: Auth,
    Query(q): Query<ObjectSearchQuery>,
) -> ApiResult<Json<Vec<ObjectTextHitView>>> {
    auth.require(Action::Read, "storage")?;
    let ws = auth.principal().workspace_id;
    let cap = u32::try_from(DEFAULT_OBJECT_SEARCH_LIMIT).unwrap_or(50);
    let limit = i64::from(q.limit.map(|n| n.clamp(1, cap)).unwrap_or(cap));
    let hits = state
        .store()
        .objects()
        .search_text_in_workspace(ws, &q.q, limit)
        .await
        .map_err(|e| ApiError::internal(format!("searching object text: {e}")))?;
    // Resolve each hit's store so the client can download from the right backend
    // (content search spans every store; the default store leaves this empty-safe).
    let labels = bucket_labels(&state, ws).await?;
    let views = hits
        .into_iter()
        .map(|h| ObjectTextHitView {
            id: h.id,
            store: labels
                .get(&h.bucket_id)
                .map(|(_, s)| s.clone())
                .unwrap_or_default(),
            key: h.key,
            content_type: h.content_type,
            excerpt: h.excerpt,
        })
        .collect();
    Ok(Json(views))
}

/// The largest extracted-text payload `GET …/text` returns (1 MiB) — generous for
/// viewing in the Files panel (≈ a 400-page book) while keeping the response
/// bounded, in line with the codebase's no-unbounded-read principle (§18). Past
/// it the text is truncated on a UTF-8 char boundary and `truncated` is set.
const MAX_OBJECT_TEXT_BYTES: usize = 1 << 20;

/// Truncate `text` to [`MAX_OBJECT_TEXT_BYTES`] on a UTF-8 char boundary; returns
/// the (possibly shortened) text and whether it was truncated.
fn cap_object_text(text: &str) -> (String, bool) {
    if text.len() <= MAX_OBJECT_TEXT_BYTES {
        return (text.to_string(), false);
    }
    let mut end = MAX_OBJECT_TEXT_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

/// Query for the storage listings (`GET /storage/objects` + `/storage/catalogue`)
/// — `?prefix=` filters by key prefix. `?limit=` caps the **catalogue** result
/// (clamps to `[1, DEFAULT_OBJECT_LIMIT]`, default cap); the backend `objects`
/// listing ignores it.
#[derive(Debug, Default, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub prefix: String,
    #[serde(default)]
    pub limit: Option<u32>,
    /// Which backend to list (`GET /storage/objects` only; the catalogue spans
    /// all). Omitted → the default store.
    #[serde(default)]
    pub store: Option<String>,
}

/// `?store=` selector for the per-object blob routes (upload / download / delete):
/// which backend the file lives on (SOUL §9). Omitted → the default store.
#[derive(Debug, Default, Deserialize)]
pub struct StoreQuery {
    #[serde(default)]
    pub store: Option<String>,
}

/// The result of an upload: the stored object's metadata + how many automations
/// the `StorageObject` trigger fired.
#[derive(Debug, Serialize)]
pub struct UploadResult {
    pub object: ObjectMeta,
    pub fired: usize,
}


/// `POST /storage/stores/{name}/scan` — reconcile a store's catalogue with its
/// backend (`storage:write`): catalogue + index new/changed files, purge vanished
/// ones. The manual "Scan / index" action behind the Files panel; returns a
/// [`ScanReport`]. Indexing runs asynchronously (the §10 ingest worker), so a hit's
/// "Indexed ✓" badge appears shortly after, not in this response.
async fn scan_route(
    State(state): State<AppState>,
    auth: Auth,
    Path(name): Path<String>,
) -> ApiResult<Json<ScanReport>> {
    auth.require(Action::Write, "storage")?;
    let ws = auth.principal().workspace_id;
    let handle = resolve(&state, ws, None, Some(&name)).await?;
    let report = scan_store(state.store(), ws, &handle, "", ScanEvents::Fire).await?;
    Ok(Json(report))
}

async fn list(
    State(state): State<AppState>,
    auth: Auth,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<ObjectMeta>>> {
    let p = auth.principal();
    auth.require(Action::Read, "storage")?;
    let storage = resolve(&state, p.workspace_id, Some(p.user_id), q.store.as_deref()).await?;
    // Scope the listing to this workspace's namespace so a tenant can only see
    // its own blobs (SOUL §18); the physical `<workspace_id>/…` prefix is stripped
    // from each returned key so callers see their user-facing keys. A *browse*
    // store skips both steps and lists its raw root (its files as they are on disk).
    let ws = p.workspace_id;
    let scoped_prefix = storage.physical_key(ws, &q.prefix);
    let stream = storage
        .backend
        .list(&scoped_prefix)
        .await
        .map_err(|e| ApiError::internal(format!("listing objects: {e}")))?;
    // Bound the listing so a huge bucket can't stream unbounded into memory; the
    // lazy `take` also stops pulling from the backend early. Mirrors the
    // catalogue's `DEFAULT_OBJECT_LIMIT` cap (this is the raw backend view).
    let objects: Vec<ObjectMeta> = stream
        .filter_map(|r| async move { r.ok() })
        .take(DEFAULT_OBJECT_LIMIT as usize)
        .map(move |mut m| {
            m.key = storage.user_key(ws, &m.key);
            m
        })
        .collect()
        .await;
    Ok(Json(objects))
}

/// A catalogued object (the Postgres truth row) for the catalogue REST surface:
/// the object's metadata with its bucket **name** resolved (not an opaque id) and
/// the §10 extracted-text document link surfaced. The `key` is the user-facing
/// key (the physical `<workspace_id>/…` namespace is never exposed, §18).
#[derive(Debug, Serialize)]
pub struct ObjectView {
    pub id: ObjectId,
    pub bucket: String,
    /// The **store** name (the `?store=` selector) the object lives on — derived
    /// from its bucket's connection. May differ from `bucket` for the default
    /// store (whose catalogue bucket can carry a configured name). The Files panel
    /// uses this for per-object download/delete.
    pub store: String,
    pub key: String,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    pub last_modified: chrono::DateTime<chrono::Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// The extracted-text [`Document`](catalerum_core::model::Document) (§10),
    /// present once the object has been ingested.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extracted_text_id: Option<DocumentId>,
}

/// `GET /storage/buckets` — list the workspace's catalogued buckets (Postgres
/// truth, `storage:read`). Independent of the blob backend: returns whatever has
/// been catalogued, even if storage is not currently configured.
async fn list_buckets(State(state): State<AppState>, auth: Auth) -> ApiResult<Json<Vec<Bucket>>> {
    let p = auth.principal();
    auth.require(Action::Read, "storage")?;
    let buckets = state
        .store()
        .buckets()
        .list_by_workspace(p.workspace_id)
        .await
        .map_err(|e| ApiError::internal(format!("listing buckets: {e}")))?;
    Ok(Json(buckets))
}

/// `GET /storage/catalogue?prefix=…` — list the workspace's catalogued objects
/// (Postgres truth), newest-modified first, prefix-filtered on the user-facing
/// key, each carrying its bucket name + the §10 extracted-text link. Distinct
/// from `GET /storage/objects`, which lists the blob backend's filesystem; this
/// is the queryable catalogue (`storage:read`), workspace-scoped (§18).
/// Map every catalogued bucket in a workspace to `(bucket name, store name)` —
/// resolving `bucket → connection → store` so a catalogue or content-search
/// response can carry both the bucket label and the `?store=` selector each
/// object lives on (which can differ for the default store, SOUL §9).
async fn bucket_labels(
    state: &AppState,
    workspace_id: WorkspaceId,
) -> ApiResult<HashMap<BucketId, (String, String)>> {
    crate::tools::bucket_store_map(state.store(), Some(state.storage()), workspace_id)
        .await
        .map_err(|e| ApiError::internal(format!("resolving bucket store names: {e}")))
}

async fn list_catalogue(
    State(state): State<AppState>,
    auth: Auth,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<ObjectView>>> {
    let p = auth.principal();
    auth.require(Action::Read, "storage")?;
    let store = state.store();
    // Each object carries both its bucket label and the `?store=` selector it lives
    // on (which can differ for the default store).
    let labels = bucket_labels(&state, p.workspace_id).await?;
    // Prefix + bound are applied in SQL (the bound *after* the prefix filter), so
    // a large catalogue can't return an unbounded set.
    let limit = i64::from(
        q.limit
            .map(|n| n.clamp(1, DEFAULT_OBJECT_LIMIT as u32))
            .unwrap_or(DEFAULT_OBJECT_LIMIT as u32),
    );
    let objects = store
        .objects()
        .list_by_workspace(p.workspace_id, &q.prefix, limit)
        .await
        .map_err(|e| ApiError::internal(format!("listing catalogued objects: {e}")))?;
    let views: Vec<ObjectView> = objects
        .into_iter()
        .map(|o| {
            let (bucket, store_name) = labels.get(&o.bucket_id).cloned().unwrap_or_default();
            ObjectView {
                id: o.id,
                bucket,
                store: store_name,
                key: o.key,
                size: o.size,
                content_type: o.content_type,
                etag: o.etag,
                last_modified: o.last_modified,
                sha256: o.sha256,
                extracted_text_id: o.extracted_text_id,
            }
        })
        .collect();
    Ok(Json(views))
}

/// The §10 extracted text for a catalogued object (`GET /storage/catalogue/{id}/text`).
/// `has_text` is `false` (with an empty `text`) when the object exists but has not
/// been ingested yet, or its extraction yielded nothing.
#[derive(Debug, Serialize)]
pub struct ObjectTextView {
    pub id: ObjectId,
    pub key: String,
    pub has_text: bool,
    pub text: String,
    /// Whether `text` was capped at [`MAX_OBJECT_TEXT_BYTES`].
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// `GET /storage/catalogue/{id}/text` — the extracted-text document for one
/// catalogued object (§10, `storage:read`). The object's existence in **this**
/// workspace is confirmed first (`NotFound` otherwise — never leaks another
/// tenant's object), then its extracted-text [`Document`] is returned (empty +
/// `has_text:false` when the object is not yet ingested). Bounded by
/// [`MAX_OBJECT_TEXT_BYTES`]. This is the read side of the Files panel's
/// "Indexed ✓" badge — the same document the `read_object` tool surfaces to agents.
async fn object_text(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<ObjectId>,
) -> ApiResult<Json<ObjectTextView>> {
    auth.require(Action::Read, "storage")?;
    let ws = auth.principal().workspace_id;
    let store = state.store();
    // Tenant scoping: a NotFound here means the object isn't in this workspace.
    let object = store
        .objects()
        .get(ws, id)
        .await
        .map_err(|_| ApiError::NotFound)?;
    let doc = store
        .documents()
        .get_by_source(ws, &SourceRef::Object { id })
        .await
        .map_err(|e| ApiError::internal(format!("loading extracted text: {e}")))?;
    let (text, truncated) = cap_object_text(doc.as_ref().map(|d| d.text.as_str()).unwrap_or(""));
    Ok(Json(ObjectTextView {
        id,
        key: object.key,
        has_text: doc.is_some(),
        text,
        truncated,
        summary: doc.and_then(|d| d.summary),
    }))
}

async fn upload(
    State(state): State<AppState>,
    auth: Auth,
    Path(key): Path<String>,
    Query(sq): Query<StoreQuery>,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<UploadResult>)> {
    let p = auth.principal();
    auth.require(Action::Write, "storage")?;
    let storage = resolve(&state, p.workspace_id, Some(p.user_id), sq.store.as_deref()).await?;
    let key = key.trim_matches('/').to_string();
    validate_object_key(&key)?;
    // The bytes live under the workspace-namespaced physical key (SOUL §18) — or
    // the raw key on a browse store; the catalogue, the response, and the trigger
    // all keep the user-facing `key`.
    let physical = storage.physical_key(p.workspace_id, &key);
    let len = body.len() as u64;
    let meta = PutMeta {
        content_type: None,
        content_length: Some(len),
    };
    let bytes = body.to_vec();
    let data = stream::once(async move { Ok(bytes) }).boxed();
    storage
        .backend
        .put(&physical, data, meta)
        .await
        .map_err(map_storage_err)?;
    let mut object = storage
        .backend
        .stat(&physical)
        .await
        .map_err(map_storage_err)?;
    // Present + catalogue the user-facing key, never the physical namespaced one.
    object.key = key.clone();

    // Catalogue the object in Postgres truth (best-effort) so it's queryable
    // (`query_structured` recent_objects, §6.5) and ingestable (§10) — the
    // storage analogue of how notes/events become catalogued things (§1/§9). On
    // success, enqueue its §10 ingest (extract text → `documents` + embed): the
    // worker reconciles to the object's current state, so a re-upload re-extracts.
    // The returned flag says whether this key was new (`created`) or an overwrite of
    // an existing one (`updated`) — which trigger the upload fires.
    let was_created = match catalogue_object(
        state.store(),
        p.workspace_id,
        &storage.connection,
        &storage.bucket,
        &object,
    )
    .await
    {
        Some((object_id, was_created)) => {
            if let Err(e) =
                catalerum_ingest::enqueue_ingest_object(state.store(), p.workspace_id, object_id)
                    .await
            {
                tracing::warn!(error = %e, %object_id, "failed to enqueue object ingest (object catalogued)");
            }
            was_created
        }
        None => true,
    };

    // Fire `StorageObject` automations for the upload (best-effort: a dispatch
    // failure never fails the upload — the object is already stored).
    let event = if was_created { "created" } else { "updated" };
    let fired = dispatch_storage_event(
        state.store(),
        p.workspace_id,
        &storage.bucket,
        &key,
        object.content_type.as_deref(),
        event,
    )
    .await;
    Ok((StatusCode::CREATED, Json(UploadResult { object, fired })))
}

/// `GET /storage/objects/{key}` — download an object's bytes (`storage:read`).
/// The `Content-Type` is the stored object's guessed type. `404` if absent.
async fn download(
    State(state): State<AppState>,
    auth: Auth,
    Path(key): Path<String>,
    Query(sq): Query<StoreQuery>,
) -> ApiResult<Response> {
    let p = auth.principal();
    auth.require(Action::Read, "storage")?;
    let storage = resolve(&state, p.workspace_id, Some(p.user_id), sq.store.as_deref()).await?;
    let key = key.trim_matches('/');
    validate_object_key(key)?;
    // Read from this workspace's namespace — a foreign key resolves to a path that
    // does not exist for this tenant, so cross-tenant reads `404` (SOUL §18). A
    // browse store reads its raw key (the file as it sits on disk).
    let physical = storage.physical_key(p.workspace_id, key);
    let meta = storage
        .backend
        .stat(&physical)
        .await
        .map_err(map_storage_err)?;
    let mut stream = storage
        .backend
        .get(&physical)
        .await
        .map_err(map_storage_err)?;
    let mut bytes = Vec::with_capacity(meta.size as usize);
    while let Some(chunk) = stream.next().await {
        bytes.extend(chunk.map_err(map_storage_err)?);
    }
    let content_type = meta
        .content_type
        .unwrap_or_else(|| "application/octet-stream".to_string());
    Ok(([(header::CONTENT_TYPE, content_type)], bytes).into_response())
}

/// `DELETE /storage/objects/{key}` — remove an object (`storage:write`; object
/// management is a write op, consistent with the API gating no handler on
/// `Delete`). Idempotent: `204` whether or not the object existed.
async fn remove(
    State(state): State<AppState>,
    auth: Auth,
    Path(key): Path<String>,
    Query(sq): Query<StoreQuery>,
) -> ApiResult<StatusCode> {
    let p = auth.principal();
    auth.require(Action::Write, "storage")?;
    let storage = resolve(&state, p.workspace_id, Some(p.user_id), sq.store.as_deref()).await?;
    let key = key.trim_matches('/');
    delete_object_at(state.store(), &storage, p.workspace_id, key).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Previews — a document → a fitted image (§9/§10)
// ---------------------------------------------------------------------------

/// Cap the buffered body of an ad-hoc `POST /storage/preview` upload. Generous
/// (office/PDF documents can be large) while still refusing an unbounded-upload
/// OOM before the bytes are read (axum's global default is a too-small 2 MiB).
const MAX_PREVIEW_BYTES: usize = 64 * 1024 * 1024;

/// Query for the preview routes: output `size` (longest-side bound, clamped to
/// `[preview].hard_max_dimension`), `fmt` (`webp`/`png`/`jpeg`), `page`
/// (1-indexed; paged documents), and — for the stored-object route — `store`.
#[derive(Debug, Default, Deserialize)]
struct PreviewQuery {
    #[serde(default)]
    store: Option<String>,
    #[serde(default)]
    size: Option<u32>,
    #[serde(default)]
    fmt: Option<String>,
    #[serde(default)]
    page: Option<u32>,
}

/// `GET /storage/preview/{key}?store=&size=&fmt=&page=` — render a stored object
/// to a fitted image (`storage:read`). Reads the object's bytes, then previews
/// them; the type is the backend's report, or an extension guess when absent.
async fn preview_object(
    State(state): State<AppState>,
    auth: Auth,
    Path(key): Path<String>,
    Query(q): Query<PreviewQuery>,
) -> ApiResult<Response> {
    let p = auth.principal();
    auth.require(Action::Read, "storage")?;
    let key = key.trim_matches('/');
    let (bytes, reported) = read_object_bytes(
        state.storage(),
        state.store(),
        p.workspace_id,
        Some(p.user_id),
        (q.store.as_deref(), key),
    )
    .await?;
    let content_type = resolve_preview_type(reported.as_deref(), key);
    render_preview(&state, bytes, &content_type, &q).await
}

/// `POST /storage/preview?size=&fmt=&page=` — render an ad-hoc document from the
/// request body to a fitted image (`storage:read`). The `Content-Type` header
/// names the document's type.
async fn preview_upload(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    Query(q): Query<PreviewQuery>,
    body: Bytes,
) -> ApiResult<Response> {
    auth.require(Action::Read, "storage")?;
    if body.is_empty() {
        return Err(ApiError::bad_request("empty document body"));
    }
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .unwrap_or("application/octet-stream")
        .to_string();
    render_preview(&state, body.to_vec(), &content_type, &q).await
}

/// Shared preview core: resolve the effective size/format against `[preview]`
/// config (clamping the requested size to the hard ceiling), run the engine
/// chain, and stream the image back with a private cache header.
async fn render_preview(
    state: &AppState,
    document: Vec<u8>,
    content_type: &str,
    q: &PreviewQuery,
) -> ApiResult<Response> {
    let previewer = state.previewer().ok_or(ApiError::NotFound)?;
    let cfg = &state.config().preview;
    let max = effective_max_dimension(q.size, cfg);
    let format = q.fmt.as_deref().map_or_else(
        || PreviewFormat::parse_or_default(&cfg.default_format),
        PreviewFormat::parse_or_default,
    );
    let request = PreviewRequest::new(document, content_type)
        .with_max_dimension(max)
        .with_format(format)
        .with_page(q.page.unwrap_or(1));
    let resp = previewer.preview(request).await.map_err(map_preview_err)?;
    Ok((
        [
            (header::CONTENT_TYPE, resp.content_type),
            // A preview is a pure function of the object + params; let the browser
            // cache it privately (not shared — a preview can carry tenant data).
            (header::CACHE_CONTROL, "private, max-age=3600".to_string()),
        ],
        resp.image,
    )
        .into_response())
}

/// The effective longest-side bound: the requested `size` (or the config default
/// when unset), clamped into `[16, hard_max_dimension]`. The ceiling protects
/// the sandbox engine's size-capped stdout channel and bounds render cost.
fn effective_max_dimension(requested: Option<u32>, cfg: &crate::config::PreviewConfig) -> u32 {
    let ceil = cfg.hard_max_dimension.max(16);
    requested
        .filter(|v| *v > 0)
        .unwrap_or(cfg.max_dimension)
        .clamp(16, ceil)
}

/// The content type to preview a stored object as: the backend's report, unless
/// it is absent or the generic `application/octet-stream` — then guess from the
/// key's extension (so an object stored without a type still routes correctly).
fn resolve_preview_type(reported: Option<&str>, key: &str) -> String {
    let usable = reported
        .map(str::trim)
        .filter(|c| !c.is_empty() && *c != "application/octet-stream");
    match usable {
        Some(ct) => ct.to_string(),
        None => mime_guess::from_path(key)
            .first_raw()
            .unwrap_or("application/octet-stream")
            .to_string(),
    }
}

/// Map a preview engine error to HTTP: an unsupported media type reads as a
/// client error (the file simply can't be previewed); everything else via the
/// shared core-error mapping (`Invalid` → 400, timeout/provider → 500).
fn map_preview_err(e: CoreError) -> ApiError {
    match e {
        CoreError::Unsupported(m) => {
            ApiError::bad_request(format!("cannot preview this file type: {m}"))
        }
        other => ApiError::from(other),
    }
}

// ---------------------------------------------------------------------------
// Labels — user/agent tags on stored files & directories (§9)
// ---------------------------------------------------------------------------

/// Query for `GET /storage/labels` — list a store's labels for the Files panel's
/// tree badges, **or** (when `label` is set) every path across all stores
/// carrying that label (the label filter). `store` selects the backend (omitted →
/// the default store's empty selector); `prefix` restricts to paths under it.
#[derive(Debug, Default, Deserialize)]
pub struct LabelsQuery {
    #[serde(default)]
    pub store: Option<String>,
    #[serde(default)]
    pub prefix: Option<String>,
    /// When set, list every path with this exact label (ignores `store`/`prefix`).
    #[serde(default)]
    pub label: Option<String>,
}

/// Query for `GET /storage/labels/for` — the labels on one exact `(store, path)`.
#[derive(Debug, Default, Deserialize)]
pub struct LabelForQuery {
    #[serde(default)]
    pub store: Option<String>,
    pub path: String,
}

/// Body for `POST /storage/labels` — apply `label` to a `path` in `store`. `path`
/// is a user-facing key (a file's key, or a directory path); `is_dir` records
/// which. `store` omitted → the default store's empty selector.
#[derive(Debug, Deserialize)]
pub struct AddLabel {
    #[serde(default)]
    pub store: Option<String>,
    pub path: String,
    #[serde(default)]
    pub is_dir: bool,
    pub label: String,
}

/// Normalize an optional `?store=` selector to the stored form: trimmed, with an
/// omitted/blank value folding to the empty string (the default store's key), so
/// the panel and the resolver agree on how a path is scoped.
fn store_key(store: Option<&str>) -> String {
    store.map(str::trim).unwrap_or("").to_string()
}

/// `GET /storage/labels?store=&prefix=` (or `?label=`) — list labels
/// (`storage:read`, workspace-scoped). With `label` set, returns every path
/// carrying that label across all stores; otherwise a store's labels under the
/// optional `prefix`.
async fn list_labels(
    State(state): State<AppState>,
    auth: Auth,
    Query(q): Query<LabelsQuery>,
) -> ApiResult<Json<Vec<ObjectLabel>>> {
    auth.require(Action::Read, "storage")?;
    let ws = auth.principal().workspace_id;
    let labels = match q.label.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(label) => state
            .store()
            .object_labels()
            .list_by_label(ws, label, DEFAULT_LABEL_LIMIT)
            .await
            .map_err(|e| ApiError::internal(format!("listing labels: {e}")))?,
        None => state
            .store()
            .object_labels()
            .list_by_store(
                ws,
                &store_key(q.store.as_deref()),
                q.prefix.as_deref().unwrap_or(""),
                DEFAULT_LABEL_LIMIT,
            )
            .await
            .map_err(|e| ApiError::internal(format!("listing labels: {e}")))?,
    };
    Ok(Json(labels))
}

/// `GET /storage/labels/for?store=&path=` — the labels on one exact path
/// (`storage:read`, workspace-scoped).
async fn list_labels_for(
    State(state): State<AppState>,
    auth: Auth,
    Query(q): Query<LabelForQuery>,
) -> ApiResult<Json<Vec<ObjectLabel>>> {
    auth.require(Action::Read, "storage")?;
    let ws = auth.principal().workspace_id;
    let labels = state
        .store()
        .object_labels()
        .list_for(ws, &store_key(q.store.as_deref()), q.path.trim())
        .await
        .map_err(|e| ApiError::internal(format!("listing labels: {e}")))?;
    Ok(Json(labels))
}

/// `POST /storage/labels` — apply a label to a file or directory path
/// (`storage:write`). Idempotent: re-applying the same label to the same path
/// returns the existing row (`201` either way). Authored by the calling user.
async fn add_label(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<AddLabel>,
) -> ApiResult<(StatusCode, Json<ObjectLabel>)> {
    auth.require(Action::Write, "storage")?;
    let principal = auth.principal();
    let label = state
        .store()
        .object_labels()
        .add(
            principal.workspace_id,
            Author::User {
                id: principal.user_id,
            },
            &store_key(body.store.as_deref()),
            body.path.trim(),
            body.is_dir,
            body.label.trim(),
        )
        .await?;
    Ok((StatusCode::CREATED, Json(label)))
}

/// `DELETE /storage/labels/{id}` — remove a label by id (`storage:write`,
/// workspace-scoped). `404` if it doesn't exist in this workspace.
async fn delete_label(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<ObjectLabelId>,
) -> ApiResult<StatusCode> {
    auth.require(Action::Write, "storage")?;
    let ws = auth.principal().workspace_id;
    state.store().object_labels().delete(ws, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Storage backends ("stores") — the destinations a file can choose between (§9)
// ---------------------------------------------------------------------------

/// A storage backend a file can be stored to (SOUL §9), for the Files panel's
/// destination picker + the storage manager. Secrets are **never** included.
#[derive(Debug, Serialize)]
pub struct StoreView {
    /// Store name — the `?store=` selector value.
    pub name: String,
    /// Backend kind (`local` / `s3` / `webdav` / `unknown`).
    pub kind: String,
    /// `config` (declared in `[storage]` / `[storage.backends.*]`, read-only) or
    /// `runtime` (user-added — editable + deletable).
    pub source: String,
    /// Whether this is the default store (the destination when none is named).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub is_default: bool,
    /// Whether catalerum is **watching** this store — keeping its §10 index in sync
    /// with the backend (real-time for local, periodic for remote, SOUL §9/§10).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub watch: bool,
}

/// `GET /storage/stores` — list the workspace's storage backends (`storage:read`):
/// the config-defined ones (read-only) plus the runtime (user-added) ones. Config
/// stores assigned to other workspaces (`workspaces` in the backend's config) are
/// omitted (SOUL §9/§18). The default store is flagged. No secrets are returned.
async fn list_stores(State(state): State<AppState>, auth: Auth) -> ApiResult<Json<Vec<StoreView>>> {
    auth.require(Action::Read, "storage")?;
    let ws = auth.principal().workspace_id;
    let registry = state.storage();
    let vis_ws = visibility_workspace(registry, state.store(), ws).await?;
    let config = registry.infos_for(vis_ws.as_ref());
    let runtime = runtime_stores(state.storage(), state.store(), ws).await?;
    // The effective default mirrors `resolve`: the (visible) config default, else
    // the sole store across both sources, else none.
    let total = config.len() + runtime.len();
    let effective_default = registry
        .default_name()
        .filter(|d| registry.visible(d, vis_ws.as_ref()))
        .map(str::to_string)
        .or_else(|| {
            if total == 1 {
                config
                    .first()
                    .map(|(n, _)| n.clone())
                    .or_else(|| runtime.first().map(|r| r.name.clone()))
            } else {
                None
            }
        });
    let is_default = |name: &str| effective_default.as_deref() == Some(name);
    // Config stores' watch flag comes from their `[storage.backends.*]` config.
    let config_watch: HashMap<String, bool> = state
        .config()
        .storage
        .resolved_backends()
        .into_iter()
        .map(|(n, c)| (n, c.watch))
        .collect();
    let mut out: Vec<StoreView> = Vec::with_capacity(total);
    for (name, kind) in config {
        out.push(StoreView {
            is_default: is_default(&name),
            watch: config_watch.get(&name).copied().unwrap_or(false),
            name,
            kind: kind.to_string(),
            source: "config".to_string(),
        });
    }
    for rs in runtime {
        out.push(StoreView {
            is_default: is_default(&rs.name),
            watch: rs.watch,
            name: rs.name,
            kind: rs.kind,
            source: "runtime".to_string(),
        });
    }
    Ok(Json(out))
}

/// `POST /storage/stores` body: a new runtime storage backend. `config` carries the
/// backend's fields (local: `local_path`; s3: `endpoint`/`region`/`access_key`/
/// `secret_key`/`bucket`/`path_style`; webdav: `url`/`username`/`password`). An
/// optional `"browse": true` in `config` opts the store into browse mode — its raw
/// root is listed with no `<workspace_id>/` namespacing, so an existing directory's
/// files are visible (SOUL §9/§18; carried through verbatim and read by
/// [`runtime_browse`](crate::object_service::runtime_browse)). An optional `"watch": true` keeps its §10 index in sync as
/// files change (real-time for local, periodic for remote; read by [`runtime_watch`]).
#[derive(Debug, Deserialize)]
pub struct CreateStore {
    pub name: String,
    /// `local` / `s3` / `webdav` (aliases accepted; empty → inferred from fields).
    #[serde(default)]
    pub kind: String,
    /// Backend fields (and secrets — stored verbatim today, see SOUL §13).
    #[serde(default)]
    pub config: serde_json::Value,
}

/// Canonical lowercase token for a [`catalerum_storage::StorageSubKind`].
fn sub_kind_str(sub: catalerum_storage::StorageSubKind) -> &'static str {
    use catalerum_storage::StorageSubKind::{Local, WebDav, S3};
    match sub {
        Local => "local",
        S3 => "s3",
        WebDav => "webdav",
    }
}

/// `POST /storage/stores` — add a runtime storage backend (`storage:write`
/// **and** a workspace administrator): registering a backend is a
/// workspace-operational config write — it persists shared credentials (S3
/// keys / WebDAV password, verbatim per the MCP precedent, SOUL §13) and
/// provisions infrastructure every member's uploads then land in (SOUL §18/§29).
/// Persisted as a storage `Connection` whose `config` carries the backend's
/// settings. Validated by building the backend; its container is provisioned
/// best-effort. The name must not collide with a config-defined store or an
/// existing runtime one.
async fn create_store(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<CreateStore>,
) -> ApiResult<(StatusCode, Json<StoreView>)> {
    auth.require(Action::Write, "storage")?;
    auth.require_workspace_admin()?;
    let ws = auth.principal().workspace_id;
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return Err(ApiError::bad_request("store name must not be empty"));
    }
    if state.storage().get(&name).is_some() {
        return Err(ApiError::bad_request(format!(
            "`{name}` is a config-defined store; pick another name"
        )));
    }
    // Assemble the connection config: the supplied fields + the kind discriminator.
    let mut config = match body.config {
        serde_json::Value::Object(m) => m,
        serde_json::Value::Null => serde_json::Map::new(),
        _ => return Err(ApiError::bad_request("config must be a JSON object")),
    };
    if !body.kind.trim().is_empty() {
        config.insert(
            "kind".to_string(),
            serde_json::Value::String(body.kind.trim().to_string()),
        );
    }
    let probe = serde_json::Value::Object(config.clone());
    let sub = catalerum_storage::StorageSubKind::from_config(&probe).map_err(map_storage_err)?;
    let kind = sub_kind_str(sub);
    // Normalize the stored kind to its canonical token + tag the source.
    config.insert(
        "kind".to_string(),
        serde_json::Value::String(kind.to_string()),
    );
    config.insert(
        "source".to_string(),
        serde_json::Value::String("runtime".to_string()),
    );
    let config = serde_json::Value::Object(config);
    let store_watch = runtime_watch(&config);
    // Validate (and reject missing required fields) by building the backend.
    let backend = catalerum_storage::backend_from_config(&config).map_err(map_storage_err)?;
    if find_storage_connection(state.store(), ws, &name)
        .await?
        .is_some()
    {
        return Err(ApiError::bad_request(format!(
            "a storage backend named `{name}` already exists"
        )));
    }
    // Provision the container best-effort (creates the S3 bucket / WebDAV collection).
    if let Err(e) = backend.ensure_container().await {
        tracing::warn!(error = %e, store = %name, "could not ensure container for new store (it may need to pre-exist)");
    }
    // The catalogue bucket name (the backend's configured bucket, matching what
    // `resolve` later catalogues under) — read before `config` is moved into the
    // connection row.
    let bucket_name = runtime_bucket_name(&config, &name);
    let conn = state
        .store()
        .connections()
        .create(ws, ConnectionKind::Storage, &name, None, Some(config))
        .await
        .map_err(|e| ApiError::internal(format!("creating storage connection: {e}")))?;
    // Pre-create its catalogue bucket so it shows up before the first upload.
    if let Err(e) = state
        .store()
        .buckets()
        .ensure(ws, conn.id, &bucket_name, None)
        .await
    {
        tracing::warn!(error = %e, store = %name, "created store but failed to ensure its catalogue bucket");
    }
    // Index whatever's already on the new backend (a browse store's existing
    // directory, a pre-populated S3 bucket) without blocking the response: scan in
    // the background. A `watch`-enabled store would also be picked up by the watch
    // worker within a tick, but this surfaces existing files promptly.
    {
        let state = state.clone();
        let scan_name = name.clone();
        tokio::spawn(async move {
            match resolve(&state, ws, None, Some(&scan_name)).await {
                Ok(handle) => {
                    // Baseline: pre-existing files on a freshly-attached backend are
                    // the starting state, not a burst of `created` events (§9/§11).
                    if let Err(e) =
                        scan_store(state.store(), ws, &handle, "", ScanEvents::Silent).await
                    {
                        tracing::warn!(error = %e, store = %scan_name, "initial scan of new store failed");
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, store = %scan_name, "initial scan: could not resolve new store")
                }
            }
        });
    }
    Ok((
        StatusCode::CREATED,
        Json(StoreView {
            name,
            kind: kind.to_string(),
            source: "runtime".to_string(),
            is_default: false,
            watch: store_watch,
        }),
    ))
}

/// `DELETE /storage/stores/{name}` — remove a runtime storage backend
/// (`storage:write` **and** a workspace administrator — removing a shared backend
/// is workspace-operational config, SOUL §18/§29). Drops the storage `Connection`
/// (its catalogue buckets + object rows cascade); the **blobs on the backend are
/// left intact**. A config-defined store can't be deleted here (remove it from
/// the config).
async fn delete_store(
    State(state): State<AppState>,
    auth: Auth,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    auth.require(Action::Write, "storage")?;
    auth.require_workspace_admin()?;
    let ws = auth.principal().workspace_id;
    let name = name.trim();
    if state.storage().get(name).is_some() {
        return Err(ApiError::bad_request(
            "cannot delete a config-defined store; remove it from the config",
        ));
    }
    let Some(conn) = find_storage_connection(state.store(), ws, name).await? else {
        return Err(ApiError::NotFound);
    };
    state
        .store()
        .connections()
        .delete(ws, conn.id)
        .await
        .map_err(|e| match e {
            StoreError::NotFound => ApiError::NotFound,
            other => ApiError::internal(format!("deleting storage connection: {other}")),
        })?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_service::{object_changed, resolve_store};
    use crate::state::StorageRegistry;
    use catalerum_core::model::StoredObject;
    use catalerum_store::Store;

    /// A config store assigned to workspace A (`workspaces = [a.slug]`) must be
    /// invisible from workspace B end-to-end through the resolver: named lookup
    /// 404s, and default resolution skips it in favor of the sole visible store
    /// (SOUL §9/§18). DB-gated (the resolver loads the workspace row).
    #[tokio::test]
    async fn resolve_store_honors_workspace_assignment() {
        let Some(url) = std::env::var("CATALERUM_TEST_DATABASE_URL")
            .or_else(|_| std::env::var("DATABASE_URL"))
            .ok()
        else {
            eprintln!(
                "skipping resolve_store_honors_workspace_assignment: set CATALERUM_TEST_DATABASE_URL"
            );
            return;
        };
        use crate::state::ConfigStore;
        use catalerum_storage::LocalFsBackend;
        use std::sync::Arc;

        let store_db = Store::connect(&url).await.expect("connect+migrate");
        let a = store_db
            .workspaces()
            .create("wa", &format!("wa-{}", uuid::Uuid::new_v4()))
            .await
            .expect("ws a");
        let b = store_db
            .workspaces()
            .create("wb", &format!("wb-{}", uuid::Uuid::new_v4()))
            .await
            .expect("ws b");

        let tmp = tempfile::tempdir().expect("tmp");
        let config_store = |name: &str, workspaces: Vec<String>| ConfigStore {
            backend: Arc::new(LocalFsBackend::new(tmp.path().to_path_buf())),
            connection: name.to_string(),
            bucket: name.to_string(),
            kind: "local",
            namespaced: true,
            workspaces,
        };
        let mut stores = HashMap::new();
        stores.insert("shared".to_string(), config_store("shared", Vec::new()));
        stores.insert(
            "scoped".to_string(),
            config_store("scoped", vec![a.slug.clone()]),
        );
        let registry = StorageRegistry::for_test(stores, Some("scoped".to_string()));

        // Named lookup: the assigned store resolves in its workspace, 404s
        // elsewhere; the unassigned one resolves in both.
        assert!(
            resolve_store(&registry, &store_db, a.id, None, Some("scoped"))
                .await
                .is_ok()
        );
        assert!(matches!(
            resolve_store(&registry, &store_db, b.id, None, Some("scoped")).await,
            Err(ApiError::NotFound)
        ));
        for ws in [a.id, b.id] {
            assert!(
                resolve_store(&registry, &store_db, ws, None, Some("shared"))
                    .await
                    .is_ok()
            );
        }
        // Default resolution: in A the config default ("scoped") wins; in B it is
        // invisible, so the sole visible store ("shared") is picked instead.
        let h = resolve_store(&registry, &store_db, a.id, None, None)
            .await
            .expect("default in a");
        assert_eq!(h.store, "scoped");
        let h = resolve_store(&registry, &store_db, b.id, None, None)
            .await
            .expect("default in b");
        assert_eq!(h.store, "shared");
    }

    #[test]
    fn runtime_watch_reads_the_flag() {
        assert!(runtime_watch(&serde_json::json!({"watch": true})));
        assert!(!runtime_watch(&serde_json::json!({"watch": false})));
        assert!(!runtime_watch(&serde_json::json!({})));
        // A non-bool value is ignored (treated as not-watching).
        assert!(!runtime_watch(&serde_json::json!({"watch": "yes"})));
    }

    #[test]
    fn object_changed_detects_new_and_modified_objects() {
        use catalerum_core::{BucketId, ObjectId, WorkspaceId};
        let ws = WorkspaceId::from_uuid(uuid::Uuid::nil());
        let t0 = chrono::DateTime::<chrono::Utc>::from_timestamp(1_000, 0).unwrap();
        let stored = |size: u64, etag: &str, ts: chrono::DateTime<chrono::Utc>| StoredObject {
            id: ObjectId::from_uuid(uuid::Uuid::nil()),
            workspace_id: ws,
            bucket_id: BucketId::from_uuid(uuid::Uuid::nil()),
            key: "a.txt".into(),
            size,
            content_type: None,
            etag: Some(etag.into()),
            last_modified: ts,
            sha256: None,
            extracted_text_id: None,
        };
        let meta = |size: u64, etag: &str, ts: chrono::DateTime<chrono::Utc>| ObjectMeta {
            key: "a.txt".into(),
            size,
            etag: Some(etag.into()),
            content_type: None,
            last_modified: ts,
        };
        let m = meta(10, "10-abc", t0);
        // A brand-new object (no catalogue row) is always "changed".
        assert!(object_changed(None, &m));
        // Identical size + etag + mtime → unchanged (no re-ingest on re-scan).
        assert!(!object_changed(Some(&stored(10, "10-abc", t0)), &m));
        // Any of size / etag / mtime differing → changed.
        assert!(object_changed(Some(&stored(11, "10-abc", t0)), &m));
        assert!(object_changed(Some(&stored(10, "10-xyz", t0)), &m));
        let t1 = chrono::DateTime::<chrono::Utc>::from_timestamp(2_000, 0).unwrap();
        assert!(object_changed(Some(&stored(10, "10-abc", t1)), &m));
    }

    #[test]
    fn list_query_defaults_to_empty_prefix() {
        let q: ListQuery = serde_json::from_str("{}").unwrap();
        assert_eq!(q.prefix, "");
        let q2: ListQuery = serde_json::from_str(r#"{"prefix":"notes/"}"#).unwrap();
        assert_eq!(q2.prefix, "notes/");
    }

    #[test]
    fn runtime_bucket_name_prefers_config_bucket_else_store_name() {
        // An S3 runtime store catalogues under its configured bucket (so the
        // catalogue label + StorageObject trigger match the physical bucket).
        assert_eq!(
            runtime_bucket_name(
                &serde_json::json!({"kind":"s3","bucket":"my-s3-bucket"}),
                "store1"
            ),
            "my-s3-bucket"
        );
        // Local / WebDAV stores have no bucket → fall back to the store name.
        assert_eq!(
            runtime_bucket_name(
                &serde_json::json!({"kind":"local","local_path":"/d"}),
                "archive"
            ),
            "archive"
        );
        // A blank/whitespace bucket is ignored (falls back to the store name).
        assert_eq!(
            runtime_bucket_name(&serde_json::json!({"bucket":"   "}), "s"),
            "s"
        );
    }

    #[test]
    fn validate_object_key_accepts_normal_paths_and_rejects_traversal() {
        // Normal relative keys are fine — the common case.
        for ok in ["a.txt", "docs/readme.md", "a/b/c/d.bin"] {
            assert!(validate_object_key(ok).is_ok(), "{ok} should be accepted");
        }
        // Empty, absolute, a leading `.`/`..`, and any `..` segment are rejected the
        // same way the local-fs backend rejects them (interior `.` like `a/./b`
        // normalizes to `a/b` per `Path::components`, so it's harmless and allowed —
        // matching local). The contract is thus backend-independent.
        for bad in ["", "..", ".", "../escape", "a/../../b", "/abs"] {
            assert!(
                validate_object_key(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn cap_object_text_truncates_on_a_char_boundary() {
        // Short text passes through untouched.
        let (out, trunc) = cap_object_text("hello");
        assert_eq!(out, "hello");
        assert!(!trunc);
        // Over the cap → truncated, and never split mid-codepoint (the result is
        // valid UTF-8 by construction since it's a `&str` slice on a boundary).
        let big = "é".repeat(MAX_OBJECT_TEXT_BYTES); // 2 bytes each → 2 MiB
        let (out, trunc) = cap_object_text(&big);
        assert!(trunc);
        assert!(out.len() <= MAX_OBJECT_TEXT_BYTES);
        assert!(big.starts_with(&out));
    }

    /// Store-lifecycle writes (`POST`/`DELETE /storage/stores`) are gated on a
    /// workspace administrator: both handlers call `auth.require_workspace_admin()`
    /// first, so a plain Member/Viewer is `403` and an Owner/Admin passes —
    /// regardless of the deployment mode (SOUL §18/§29). (Per-object uploads /
    /// labels stay member-writable on `storage:write`; only backend registration
    /// is admin-gated.)
    #[test]
    fn store_lifecycle_requires_workspace_admin() {
        use crate::auth::Auth;
        use catalerum_core::model::Role;
        use catalerum_core::{UserId, WorkspaceId};
        let auth = |role| {
            Auth::from_principal(catalerum_iam::Principal::new(
                UserId::new(),
                WorkspaceId::new(),
                role,
            ))
        };
        assert!(auth(Role::Owner).require_workspace_admin().is_ok());
        assert!(auth(Role::Admin).require_workspace_admin().is_ok());
        assert!(matches!(
            auth(Role::Member).require_workspace_admin(),
            Err(ApiError::Forbidden(_))
        ));
        assert!(matches!(
            auth(Role::Viewer).require_workspace_admin(),
            Err(ApiError::Forbidden(_))
        ));
    }
}
