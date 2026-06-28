//! Semantic search / indexing / email search / graph query tools.

use super::*;

/// `search_semantic` — semantic (vector) retrieval over the workspace's embedded
/// chunks (SOUL §6.4/§6.5/§7). Embeds the query through the [`Embedder`], runs a
/// filtered ANN search in Qdrant scoped to the caller's workspace, and returns
/// the matched chunks (text + source + score) for the agent to ground on.
pub(crate) struct SearchSemanticTool {
    pub(crate) search: SemanticSearch,
    /// Used to re-check the visibility of memory-kind hits before returning them
    /// (a private memory's vector must not leak across users, §18/§22).
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for SearchSemanticTool {
    fn name(&self) -> &str {
        "search_semantic"
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Search, "vector")
    }

    fn description(&self) -> &str {
        "Semantic search over the user's notes and uploaded documents by meaning \
         (not keywords). Returns the most relevant text chunks with their source and \
         a similarity score — use it to ground answers in the user's own content. \
         Email is NOT included here: search mail with `search_emails`. To read one \
         source in full once a hit identifies it, use `read_note` / `read_object` / \
         `read_email`; for exact records (by tag, date, status) use `query_structured`."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Natural-language query to search for." },
                "limit": {
                    "type": "integer",
                    "description": "Max results to return (1-20, default 8).",
                    "minimum": 1,
                    "maximum": 20
                },
                "kinds": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional source-kind filter, e.g. [\"note\"]. Omit for all."
                },
                "bucket_name": {
                    "type": "string",
                    "description": "Optional: restrict to uploaded files in this storage bucket."
                },
                "key_prefix": {
                    "type": "string",
                    "description": "Optional: restrict to files whose key starts with this path prefix (a subdir), e.g. \"docs/\". Scopes search to one folder of files."
                }
            },
            "required": ["query"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let query = required_str(&args, "query")?;
        let limit = opt_clamped_u64(&args, "limit", 8, 20);
        let kinds = opt_str_vec(&args, "kinds");
        let bucket_name = opt_str_some(&args, "bucket_name");
        let key_prefix = opt_str_some(&args, "key_prefix");

        // Vectorise the query with the same embedding model the index was built
        // with (SOUL §6.4); the first (only) vector is the query embedding.
        let resp = self
            .search
            .embedder
            .embed(EmbeddingRequest::single(&self.search.embed_model, query))
            .await?;
        let vector = resp
            .embeddings
            .into_iter()
            .next()
            .map(|e| e.vector)
            .ok_or_else(|| Error::provider("embedder returned no query vector"))?;

        // Over-fetch (4×, bounded) so the post-fetch filters below — hidden memories
        // and email — don't under-fill the result below `limit` when those crowd the
        // top hits; we truncate back to `limit` after filtering. Mirrors
        // `recall_memory_texts`. (`kinds` is pushed into the index filter, so it
        // doesn't need the headroom; memory-visibility can't be — it's per-user, §22.)
        let scan = limit.saturating_mul(4).min(80);
        let q = SearchQuery::new(vector, scan).with_filter(SearchFilter {
            kinds,
            bucket_name,
            key_prefix,
            ..Default::default()
        });
        let hits = self
            .search
            .vector
            .search(ws, &q)
            .await
            .map_err(|e| Error::provider(format!("vector search failed: {e}")))?;
        // Memory vectors carry no visibility in the index, so drop any memory hit
        // the caller may not see (a private memory must never leak, §18/§22).
        let mut hits = drop_hidden_memory_hits(&self.store, ws, ctx.user_id, hits).await;
        // Email is its own sensitive domain (`email:read`): it is reachable only via
        // `search_emails`, never surfaced here under the broader `vector:search`.
        // Drop email-kind hits even when the caller passes no `kinds` filter, so a
        // principal without `email:read` can't read mail through this tool (§18/§19)
        // — the analogue of the memory-visibility filter above.
        hits.retain(|h| !matches!(h.payload.source, SourceRef::Email { .. }));
        // Truncate to the caller's requested limit after filtering (over-fetched above).
        hits.truncate(limit as usize);

        let results: Vec<Json> = hits
            .into_iter()
            .map(|h| {
                json!({
                    "text": h.payload.text,
                    "source": h.payload.source,
                    "score": h.score,
                })
            })
            .collect();
        Ok(json!({ "hits": results }))
    }
}

/// `index_document` — (re-)index one document source into the derived vector
/// (embeddings) index (SOUL §6.4/§10) so its text becomes semantically searchable
/// via `search_semantic`. It enqueues the durable, **idempotent** embed→upsert
/// pipeline for the named source (a stored object, a note, or a memory): the
/// source's current text is chunked, embedded, and upserted to Qdrant, replacing
/// any prior vectors for it — so re-running never duplicates and an edited/deleted
/// source reconciles cleanly. The actual embedding happens in the background worker
/// (this tool only enqueues), so it returns the queued job id, not the vectors.
///
/// Gated on `vector:write` (a Viewer is denied). Registered only when a vector
/// index is configured (`[qdrant].enabled`), alongside `search_semantic` — indexing
/// into an absent vector store is a no-op the worker couldn't serve. The
/// `IndexDocument` automation action (SOUL §11) dispatches through this same tool.
pub(crate) struct IndexDocumentTool {
    pub(crate) store: Store,
    /// The derived vector index — used only by the `operation:"delete"` de-index
    /// path (`delete_by_key`), where a removed file's object row (and thus its
    /// typed id) may already be gone.
    pub(crate) vector: VectorStore,
}

#[async_trait]
impl Tool for IndexDocumentTool {
    fn name(&self) -> &str {
        "index_document"
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "vector")
    }

    fn description(&self) -> &str {
        "Index, re-index, or de-index one of the user's documents in the semantic \
         search index. Identify the source either by `id` (an \"object\"/\"note\"/\"memory\" \
         row id) OR — for an uploaded file — by `bucket` + `key` (the file's storage \
         path), which is what a storage-change trigger provides. `operation` defaults \
         to \"index\" (embed in the background; idempotent — re-indexing replaces a \
         source's vectors, never duplicates); pass \"delete\" to remove a file's vectors \
         when it was deleted. Typical use: a storage trigger routes created/updated \
         files here to index and deleted files here to delete."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "source": {
                    "type": "string",
                    "enum": ["object", "note", "memory"],
                    "description": "Which kind of source: \"object\" (an uploaded file), \"note\", or \"memory\". Defaults to \"object\" when `bucket`+`key` are given."
                },
                "id": {
                    "type": "string",
                    "description": "The source row id (object/note/memory id matching `source`). Provide this OR `bucket`+`key`."
                },
                "bucket": {
                    "type": "string",
                    "description": "For an uploaded file: the storage bucket name (as provided by a storage-change trigger). Use with `key` instead of `id`."
                },
                "key": {
                    "type": "string",
                    "description": "For an uploaded file: the object key/path within `bucket` (as provided by a storage-change trigger)."
                },
                "operation": {
                    "type": "string",
                    "enum": ["index", "delete"],
                    "description": "\"index\" (default) to embed/re-embed, or \"delete\" to remove the source's vectors (a deleted file)."
                }
            }
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let operation = {
            let op = opt_str(&args, "operation").to_ascii_lowercase();
            if op.is_empty() {
                "index".to_string()
            } else {
                op
            }
        };

        // Resolve the target. Two addressing modes: a typed `id` (+ `source`), or an
        // uploaded file's `bucket` + `key` (what a storage trigger carries). The
        // latter always denotes an object; we resolve it to its catalogued ObjectId.
        let bucket = opt_str_some(&args, "bucket");
        let key = opt_str_some(&args, "key");

        // The de-index path keys on (bucket, key) directly so it works even when the
        // object row is already gone (the usual case for a "deleted" trigger event).
        if operation == "delete" {
            if let (Some(bucket), Some(key)) = (&bucket, &key) {
                self.vector
                    .delete_by_key(ws, bucket, key)
                    .await
                    .map_err(|e| Error::provider(format!("failed to de-index: {e}")))?;
                return Ok(json!({ "deleted": true, "bucket": bucket, "key": key }));
            }
            // Fall back to id-based delete via the idempotent ingest job, which
            // purges a source found deleted (note/memory/object by id).
        }

        // Determine (source, typed id). bucket+key → resolve the object row.
        let (source, id) = if let (Some(bucket), Some(key)) = (&bucket, &key) {
            let b = self
                .store
                .buckets()
                .get_by_name(ws, bucket)
                .await
                .map_err(|e| Error::invalid(format!("unknown bucket `{bucket}`: {e}")))?;
            let obj = self
                .store
                .objects()
                .get_by_key(ws, b.id, key)
                .await
                .map_err(|e| Error::invalid(format!("no object at `{bucket}/{key}`: {e}")))?;
            ("object".to_string(), obj.id.to_string())
        } else {
            let source = required_str(&args, "source")?.to_ascii_lowercase();
            let id = required_str(&args, "id")?.to_string();
            (source, id)
        };

        // Enqueue the matching durable, idempotent ingest job (the same producers
        // the note/object/memory write paths use). For "delete" via id, the job
        // reconciles a source found deleted by purging it. A bad id is a caller
        // error (`invalid`); a queue failure is a provider error.
        let job_id = match source.as_str() {
            "object" => {
                let oid = id
                    .parse::<ObjectId>()
                    .map_err(|e| Error::invalid(format!("invalid object id `{id}`: {e}")))?;
                catalerum_ingest::enqueue_ingest_object(&self.store, ws, oid).await
            }
            "note" => {
                let nid = id
                    .parse::<NoteId>()
                    .map_err(|e| Error::invalid(format!("invalid note id `{id}`: {e}")))?;
                catalerum_ingest::enqueue_ingest_note(&self.store, ws, nid).await
            }
            "memory" => {
                let mid = id
                    .parse::<MemoryId>()
                    .map_err(|e| Error::invalid(format!("invalid memory id `{id}`: {e}")))?;
                catalerum_ingest::enqueue_ingest_memory(&self.store, ws, mid).await
            }
            other => {
                return Err(Error::invalid(format!(
                    "unknown `source` `{other}`; expected one of: object, note, memory"
                )))
            }
        }
        .map_err(|e| Error::provider(format!("failed to enqueue index job: {e}")))?;
        Ok(json!({
            "enqueued": true,
            "operation": operation,
            "source": source,
            "id": id,
            "job_id": job_id.to_string(),
        }))
    }
}

/// Default cap on how many objects one `reindex_objects` call enqueues.
pub(crate) const DEFAULT_REINDEX_LIMIT: u64 = 500;
/// Hard cap on a single `reindex_objects` call, so a huge catalogue can't enqueue
/// an unbounded flood of jobs in one shot.
pub(crate) const MAX_REINDEX_LIMIT: u64 = 5000;

/// `reindex_objects` — the **bulk** companion to `index_document` (SOUL §6.4/§10):
/// (re-)index every uploaded file under a bucket / key-prefix in one call. It
/// enumerates the catalogued objects (optionally narrowed to a `bucket` and/or a
/// `key_prefix` subdir) and enqueues the durable, idempotent embed pipeline for
/// each — so re-running never duplicates. Use it to index a whole wiki/folder that
/// was just copied in, or to rebuild the index after an upgrade that changed what
/// is stored per vector. Gated on `vector:write`; registered only with a vector
/// index configured (`[qdrant].enabled`).
pub(crate) struct ReindexObjectsTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for ReindexObjectsTool {
    fn name(&self) -> &str {
        "reindex_objects"
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "vector")
    }

    fn description(&self) -> &str {
        "Bulk (re-)index every uploaded file under a bucket / folder into the \
         semantic search index — the batch companion to `index_document`. Narrow \
         with `bucket` and/or `key_prefix` (a path prefix like \"wiki/\") to index one \
         folder, or omit both to index all catalogued files. Each file's embed runs \
         idempotently in the background (re-indexing replaces its vectors, never \
         duplicates). Use after copying a whole wiki/folder in, or to rebuild the \
         index. Bounded by `limit`."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "bucket": {
                    "type": "string",
                    "description": "Optional: restrict to files in this storage bucket."
                },
                "key_prefix": {
                    "type": "string",
                    "description": "Optional: restrict to files whose key starts with this path prefix (a subdir), e.g. \"wiki/\"."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max files to enqueue (1-5000, default 500).",
                    "minimum": 1,
                    "maximum": 5000
                }
            }
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let key_prefix = opt_str(&args, "key_prefix");
        let bucket = opt_str_some(&args, "bucket");
        let limit = opt_clamped_u64(&args, "limit", DEFAULT_REINDEX_LIMIT, MAX_REINDEX_LIMIT);

        // Resolve the optional bucket filter to a bucket id up front (an unknown
        // bucket is a caller error, not an empty result).
        let bucket_id = match &bucket {
            Some(name) => Some(
                self.store
                    .buckets()
                    .get_by_name(ws, name)
                    .await
                    .map_err(|e| Error::invalid(format!("unknown bucket `{name}`: {e}")))?
                    .id,
            ),
            None => None,
        };

        // List objects under the key prefix (empty = all), then enqueue one
        // idempotent ingest job per object (filtering to the bucket if requested).
        let objects = self
            .store
            .objects()
            .list_by_workspace(ws, &key_prefix, limit as i64)
            .await
            .map_err(|e| Error::provider(format!("listing objects: {e}")))?;

        let mut enqueued = 0u64;
        for object in objects {
            if let Some(bid) = bucket_id {
                if object.bucket_id != bid {
                    continue;
                }
            }
            catalerum_ingest::enqueue_ingest_object(&self.store, ws, object.id)
                .await
                .map_err(|e| Error::provider(format!("failed to enqueue reindex job: {e}")))?;
            enqueued += 1;
        }

        Ok(json!({
            "enqueued": enqueued,
            "bucket": bucket,
            "key_prefix": key_prefix,
        }))
    }
}

/// `search_emails` — semantic (vector) search over the workspace's **ingested
/// email** by meaning (SOUL §7/§28/§6.5): the semantic complement to `get_emails`
/// (structured) and the email-scoped sibling of `search_semantic`. Embeds the
/// query, runs a `kind = email` ANN search in Qdrant scoped to the caller's
/// workspace, resolves each hit back to its email row, and returns compact email
/// summaries (mailbox / from / subject / received / unread) each with the matched
/// snippet + similarity score. Gated on `email:read` (deny-by-default, §19): email
/// is its own sensitive domain, so it is reachable here only with the email
/// capability — never through the broader `vector:search` that `search_semantic`
/// carries (which, by design, does not surface email).
pub(crate) struct SearchEmailsTool {
    pub(crate) search: SemanticSearch,
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for SearchEmailsTool {
    fn name(&self) -> &str {
        "search_emails"
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Read, "email")
    }

    fn description(&self) -> &str {
        "Semantic search over the user's ingested email by meaning (not keywords). \
         Returns the most relevant messages — mailbox, sender, subject, received \
         time, unread flag — each with the matched text snippet and a similarity \
         score. Use it to find mail by topic or intent; use get_emails for \
         recent / unread / by-sender listings."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Natural-language query to search the mail for." },
                "limit": {
                    "type": "integer",
                    "description": "Max results to return (1-20, default 8).",
                    "minimum": 1,
                    "maximum": 20
                }
            },
            "required": ["query"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let query = required_str(&args, "query")?;
        let limit = opt_clamped_u64(&args, "limit", 8, 20);

        // Vectorise the query with the index's embedding model (SOUL §6.4).
        let resp = self
            .search
            .embedder
            .embed(EmbeddingRequest::single(&self.search.embed_model, query))
            .await?;
        let vector = resp
            .embeddings
            .into_iter()
            .next()
            .map(|e| e.vector)
            .ok_or_else(|| Error::provider("embedder returned no query vector"))?;

        // Scope the ANN search to email-kind points only. Over-fetch (4×, bounded)
        // so the loop below dropping hits whose email row is gone (a vector that
        // briefly outlived its purged email) doesn't under-fill the result below
        // `limit`; we truncate back to `limit` after resolving. Mirrors
        // `search_semantic` / `recall_memory_texts`.
        let scan = limit.saturating_mul(4).min(80);
        let q = SearchQuery::new(vector, scan).with_filter(SearchFilter {
            kinds: vec!["email".to_string()],
            ..Default::default()
        });
        let hits = self
            .search
            .vector
            .search(ws, &q)
            .await
            .map_err(|e| Error::provider(format!("vector search failed: {e}")))?;

        // Resolve every hit's email in ONE query (was a `get` per hit — N+1), then
        // iterate hits in score order. A hit whose row is gone (the vector briefly
        // outlives its email after an async purge) is simply absent from the map and
        // skipped — same tolerance as before, never surfacing a phantom message.
        let ids: Vec<EmailId> = hits
            .iter()
            .filter_map(|h| match &h.payload.source {
                SourceRef::Email { id } => Some(*id),
                _ => None,
            })
            .collect();
        let by_id: std::collections::HashMap<EmailId, _> = self
            .store
            .emails()
            .get_many(ws, &ids)
            .await
            .map_err(query_err)?
            .into_iter()
            .map(|e| (e.id, e))
            .collect();

        // Index only the mailboxes the resolved hits reference → name, in ONE batched
        // query (was an unbounded `list_by_workspace` over *every* mailbox just to map
        // a handful). Each hit then carries where it lives, not an id.
        let mailbox_ids: Vec<MailboxId> = by_id
            .values()
            .map(|e| e.mailbox_id)
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        let mailbox_index: std::collections::HashMap<MailboxId, String> = self
            .store
            .mailboxes()
            .get_many(ws, &mailbox_ids)
            .await
            .map_err(query_err)?
            .into_iter()
            .map(|m| (m.id, m.name))
            .collect();

        let mut results: Vec<Json> = Vec::new();
        for h in hits {
            let SourceRef::Email { id } = &h.payload.source else {
                continue; // defensive — the kind filter should make this unreachable
            };
            let Some(email) = by_id.get(id).cloned() else {
                continue; // its row is gone (vector outlived it) — skip, don't phantom
            };
            let mut summary = email_summary(email, &mailbox_index);
            if let Json::Object(map) = &mut summary {
                map.insert("score".into(), json!(h.score));
                map.insert("snippet".into(), json!(h.payload.text));
            }
            results.push(summary);
            if results.len() >= limit as usize {
                break; // enough resolved hits; the rest were over-fetch headroom
            }
        }
        Ok(json!({ "results": results }))
    }
}

/// Drop any `memory`-kind hit not visible to `user_id`, re-checking against
/// Postgres truth (the vector index encodes no per-user visibility, §22). Non-
/// memory hits pass through unchanged; a hit whose memory is gone is dropped.
pub(crate) async fn drop_hidden_memory_hits(
    store: &Store,
    workspace_id: WorkspaceId,
    user_id: Option<UserId>,
    hits: Vec<ScoredPoint>,
) -> Vec<ScoredPoint> {
    // The semantic index carries no visibility, so re-check each memory hit
    // against Postgres truth — but in ONE batched query keyed by id, not a `get`
    // per hit on the hot chat-recall path.
    let ids: Vec<MemoryId> = hits
        .iter()
        .filter_map(|h| match &h.payload.source {
            SourceRef::Memory { id } => Some(*id),
            _ => None,
        })
        .collect();
    let by_id: std::collections::HashMap<MemoryId, _> =
        match store.memories().get_many(workspace_id, &ids).await {
            Ok(ms) => ms.into_iter().map(|m| (m.id, m)).collect(),
            // Fail closed: recall is best-effort and must never leak across
            // visibility, so on a fetch error drop every memory hit.
            Err(_) => return Vec::new(),
        };
    hits.into_iter()
        .filter(|h| match &h.payload.source {
            SourceRef::Memory { id } => by_id.get(id).is_some_and(|m| {
                matches!(m.scope, MemoryScope::Workspace)
                    || (m.user_id.is_some() && m.user_id == user_id)
            }),
            // A non-memory hit can't occur (the search filters to kind=memory),
            // but pass it through rather than silently drop, matching prior intent.
            _ => true,
        })
        .collect()
}

/// Recall up to `limit` distinct memory texts **semantically relevant** to
/// `query` and **visible** to `user_id` (SOUL §22) — the basis for auto-recall
/// into the chat system prompt. Best-effort: any embed/search failure yields an
/// empty list (it must never fail the caller's turn). Over-scans the index so
/// visibility filtering still leaves enough, then dedups by memory (a memory may
/// have several chunks) preserving best-rank order.
pub(crate) async fn recall_memory_texts(
    store: &Store,
    search: &SemanticSearch,
    workspace_id: WorkspaceId,
    user_id: Option<UserId>,
    query: &str,
    limit: usize,
) -> Vec<String> {
    if limit == 0 || query.trim().is_empty() {
        return Vec::new();
    }
    let Ok(resp) = search
        .embedder
        .embed(EmbeddingRequest::single(&search.embed_model, query))
        .await
    else {
        return Vec::new();
    };
    let Some(vector) = resp.embeddings.into_iter().next().map(|e| e.vector) else {
        return Vec::new();
    };
    let scan = (limit.saturating_mul(4)).clamp(1, 50) as u64;
    let q = SearchQuery::new(vector, scan).with_filter(SearchFilter {
        kinds: vec!["memory".to_string()],
        ..Default::default()
    });
    let Ok(hits) = search.vector.search(workspace_id, &q).await else {
        return Vec::new();
    };
    let visible = drop_hidden_memory_hits(store, workspace_id, user_id, hits).await;

    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(limit);
    for h in visible {
        let SourceRef::Memory { id } = &h.payload.source else {
            continue;
        };
        if seen.insert(*id) {
            out.push(h.payload.text);
            if out.len() >= limit {
                break;
            }
        }
    }
    out
}

/// `query_graph` — typed, **read-only**, workspace-scoped queries over the
/// derived Neo4j graph of notes and topics (SOUL §6.3/§6.5/§7). The model never
/// writes Cypher (no injection / no writes / no cross-workspace reach): it picks
/// a named `operation` and the tool runs the matching parameterized query via
/// [`GraphStore`].
pub(crate) struct QueryGraphTool {
    pub(crate) graph: GraphQuery,
}

#[async_trait]
impl Tool for QueryGraphTool {
    fn name(&self) -> &str {
        "query_graph"
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Query, "graph")
    }

    fn description(&self) -> &str {
        "Query the knowledge graph of the user's notes and topics. operation = \
         'related_notes' finds notes that share a topic with a given note (pass \
         `note_id`); operation = 'notes_by_topic' finds notes tagged with a topic \
         (pass `topic`). Use it to discover connections between notes."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": ["related_notes", "notes_by_topic"],
                    "description": "Which graph query to run."
                },
                "note_id": { "type": "string", "description": "Note id (required for related_notes)." },
                "topic": { "type": "string", "description": "Topic name (required for notes_by_topic)." },
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
        let operation = required_str(&args, "operation")?;
        let limit = opt_clamped_u64(&args, "limit", 10, 50) as i64;

        match operation.as_str() {
            "related_notes" => {
                let note = required_str(&args, "note_id")?
                    .parse::<NoteId>()
                    .map_err(|e| Error::invalid(format!("invalid note_id: {e}")))?;
                let rows = self
                    .graph
                    .related_notes(ws, note, limit)
                    .await
                    .map_err(|e| Error::provider(format!("graph query failed: {e}")))?;
                let results: Vec<Json> = rows
                    .into_iter()
                    .map(|r| {
                        json!({
                            "note_id": r.note_id,
                            "title": r.title,
                            "shared_topics": r.shared_topics,
                        })
                    })
                    .collect();
                Ok(json!({ "operation": operation, "results": results }))
            }
            "notes_by_topic" => {
                let topic = required_str(&args, "topic")?;
                let rows = self
                    .graph
                    .notes_by_topic(ws, &topic, limit)
                    .await
                    .map_err(|e| Error::provider(format!("graph query failed: {e}")))?;
                let results: Vec<Json> = rows
                    .into_iter()
                    .map(|r| json!({ "note_id": r.note_id, "title": r.title }))
                    .collect();
                Ok(json!({ "operation": operation, "results": results }))
            }
            other => Err(Error::invalid(format!(
                "unknown query_graph operation `{other}` (expected related_notes | notes_by_topic)"
            ))),
        }
    }
}
