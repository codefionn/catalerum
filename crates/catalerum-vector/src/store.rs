//! The Qdrant REST client: per-workspace collections, idempotent upsert,
//! filtered ANN search, and source-scoped deletion (SOUL §6.4).
//!
//! Qdrant is a **derived** index — everything here is rebuildable from Postgres
//! `chunks` / `memories` / `notes`. Losing the index costs a re-embed, never
//! data (§3.1). Each workspace gets its own collection (`catalerum_ws_<uuid>`),
//! and every point *also* carries a `workspace_id` payload always filtered on, so
//! cross-workspace reach is impossible by construction (§18).

use catalerum_core::{SourceRef, WorkspaceId};
use serde_json::{json, Value};

use crate::error::{Result, VectorError};
use crate::payload::{
    source_id_string, source_kind, Distance, PointPayload, ScoredPoint, SearchFilter, SearchQuery,
    VectorPoint,
};

/// A thin async client over Qdrant's REST API, scoped per workspace.
#[derive(Clone, Debug)]
pub struct VectorStore {
    http: reqwest::Client,
    /// Base URL with no trailing slash, e.g. `http://localhost:6333`.
    base: String,
    distance: Distance,
}

/// The default HTTP client for talking to Qdrant: a short **connect timeout**
/// (fail fast when the host is unreachable instead of hanging) plus a generous
/// overall **request timeout** as a backstop so a server that stalls mid-response
/// can never block an ingest/search worker indefinitely. Normal ops are
/// sub-second and per-document upserts are small, so the 60 s cap is slack.
/// Callers that need different behaviour use [`VectorStore::with_client`].
fn default_http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(60))
        .build()?)
}

impl VectorStore {
    /// Connect to the Qdrant at `base_url` (e.g. `http://localhost:6333`) with
    /// the default Cosine distance. The URL is validated but no request is made.
    pub fn new(base_url: &str) -> Result<Self> {
        Self::with_client(default_http_client()?, base_url)
    }

    /// Build a store over an existing [`reqwest::Client`] (share a connection
    /// pool, configure timeouts/proxies upstream).
    pub fn with_client(http: reqwest::Client, base_url: &str) -> Result<Self> {
        // Parse to validate; store a normalized base without the trailing slash.
        let parsed = url::Url::parse(base_url)?;
        let base = parsed.as_str().trim_end_matches('/').to_owned();
        Ok(Self {
            http,
            base,
            distance: Distance::default(),
        })
    }

    /// Override the distance metric used when *creating* new collections.
    #[must_use]
    pub fn with_distance(mut self, distance: Distance) -> Self {
        self.distance = distance;
        self
    }

    /// The Qdrant collection name for a workspace. Stable and URL-safe.
    #[must_use]
    pub fn collection_name(workspace_id: WorkspaceId) -> String {
        format!("catalerum_ws_{}", workspace_id.as_uuid().simple())
    }

    fn collection_url(&self, workspace_id: WorkspaceId) -> String {
        format!(
            "{}/collections/{}",
            self.base,
            Self::collection_name(workspace_id)
        )
    }

    /// Liveness probe — `GET /healthz`. Cheap; use it to fail fast at startup.
    pub async fn healthz(&self) -> Result<()> {
        let resp = self
            .http
            .get(format!("{}/healthz", self.base))
            .send()
            .await?;
        ok_or_api(resp).await.map(|_| ())
    }

    /// The vector width of a workspace's collection, or `None` if it does not
    /// exist yet.
    pub async fn collection_dim(&self, workspace_id: WorkspaceId) -> Result<Option<u64>> {
        let resp = self
            .http
            .get(self.collection_url(workspace_id))
            .send()
            .await?;
        if resp.status().as_u16() == 404 {
            return Ok(None);
        }
        let body: Value = ok_or_api(resp).await?.json().await?;
        let dim = body
            .pointer("/result/config/params/vectors/size")
            .and_then(Value::as_u64);
        match dim {
            Some(d) => Ok(Some(d)),
            None => Err(VectorError::Malformed(
                "collection info missing /result/config/params/vectors/size".into(),
            )),
        }
    }

    /// Idempotently ensure a workspace's collection exists with vector width
    /// `dim`. A no-op if it already exists at that width; an error if it exists
    /// at a *different* width (recreating would silently drop data, §3.1).
    pub async fn ensure_collection(&self, workspace_id: WorkspaceId, dim: u64) -> Result<()> {
        if let Some(found) = self.collection_dim(workspace_id).await? {
            if found != dim {
                return Err(VectorError::DimensionMismatch {
                    collection: Self::collection_name(workspace_id),
                    expected: dim,
                    found,
                });
            }
            return Ok(());
        }

        let body = json!({
            "vectors": { "size": dim, "distance": self.distance.as_qdrant() },
        });
        let resp = self
            .http
            .put(self.collection_url(workspace_id))
            .json(&body)
            .send()
            .await?;

        // A concurrent creator may have won the race: re-check before failing.
        if !resp.status().is_success() {
            if let Some(found) = self.collection_dim(workspace_id).await? {
                return if found == dim {
                    Ok(())
                } else {
                    Err(VectorError::DimensionMismatch {
                        collection: Self::collection_name(workspace_id),
                        expected: dim,
                        found,
                    })
                };
            }
            return Err(api_error(resp).await);
        }
        Ok(())
    }

    /// Upsert points into a workspace's collection (idempotent on point id).
    /// Empty input is a no-op. The collection must already exist
    /// ([`ensure_collection`](Self::ensure_collection)).
    pub async fn upsert(&self, workspace_id: WorkspaceId, points: &[VectorPoint]) -> Result<()> {
        if points.is_empty() {
            return Ok(());
        }
        let body = json!({
            "points": points.iter().map(VectorPoint::to_qdrant).collect::<Vec<_>>(),
        });
        let resp = self
            .http
            .put(format!(
                "{}/points?wait=true",
                self.collection_url(workspace_id)
            ))
            .json(&body)
            .send()
            .await?;
        ok_or_api(resp).await.map(|_| ())
    }

    /// Filtered ANN search within a workspace. The workspace filter is always
    /// applied; `query.filter` narrows further. A missing collection yields an
    /// empty result (the index is derived and may simply not be built yet).
    pub async fn search(
        &self,
        workspace_id: WorkspaceId,
        query: &SearchQuery,
    ) -> Result<Vec<ScoredPoint>> {
        let mut body = serde_json::Map::new();
        body.insert("vector".into(), json!(query.vector));
        body.insert("limit".into(), json!(query.limit));
        body.insert("with_payload".into(), json!(true));
        body.insert("filter".into(), query.filter.to_qdrant(workspace_id));
        if let Some(t) = query.score_threshold {
            body.insert("score_threshold".into(), json!(t));
        }

        let resp = self
            .http
            .post(format!(
                "{}/points/search",
                self.collection_url(workspace_id)
            ))
            .json(&Value::Object(body))
            .send()
            .await?;
        if resp.status().as_u16() == 404 {
            return Ok(Vec::new());
        }
        let value: Value = ok_or_api(resp).await?.json().await?;
        let hits = value
            .get("result")
            .and_then(Value::as_array)
            .ok_or_else(|| VectorError::Malformed("search response missing result array".into()))?;

        hits.iter().map(parse_scored_point).collect()
    }

    /// Count points in a workspace matching `filter` (plus the implicit
    /// workspace filter). A missing collection counts as zero.
    pub async fn count(&self, workspace_id: WorkspaceId, filter: &SearchFilter) -> Result<u64> {
        let body = json!({
            "filter": filter.to_qdrant(workspace_id),
            "exact": true,
        });
        let resp = self
            .http
            .post(format!(
                "{}/points/count",
                self.collection_url(workspace_id)
            ))
            .json(&body)
            .send()
            .await?;
        if resp.status().as_u16() == 404 {
            return Ok(0);
        }
        let value: Value = ok_or_api(resp).await?.json().await?;
        value
            .pointer("/result/count")
            .and_then(Value::as_u64)
            .ok_or_else(|| VectorError::Malformed("count response missing /result/count".into()))
    }

    /// Delete points by id. Empty input and a missing collection are no-ops.
    pub async fn delete_points(&self, workspace_id: WorkspaceId, ids: &[uuid::Uuid]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let body = json!({
            "points": ids.iter().map(ToString::to_string).collect::<Vec<_>>(),
        });
        self.delete(workspace_id, &body).await
    }

    /// Delete every point derived from a given source — the basis for
    /// re-projection: delete-then-reupsert a note/memory's chunks idempotently.
    /// A missing collection is a no-op.
    pub async fn delete_by_source(
        &self,
        workspace_id: WorkspaceId,
        source: &SourceRef,
    ) -> Result<()> {
        let filter = json!({
            "must": [
                { "key": "workspace_id", "match": { "value": workspace_id.to_string() } },
                { "key": "kind", "match": { "value": source_kind(source) } },
                { "key": "source_id", "match": { "value": source_id_string(source) } },
            ],
        });
        self.delete(workspace_id, &json!({ "filter": filter }))
            .await
    }

    /// Delete every point derived from a storage object identified by its
    /// `bucket_name` + `key` — the de-index path for a **deleted** file, where
    /// the object's row (and thus its `SourceRef` id) may already be gone, so
    /// [`delete_by_source`](Self::delete_by_source) can't be used. Matches on the
    /// denormalized storage-path payload fields. A missing collection is a no-op.
    pub async fn delete_by_key(
        &self,
        workspace_id: WorkspaceId,
        bucket_name: &str,
        key: &str,
    ) -> Result<()> {
        let filter = json!({
            "must": [
                { "key": "workspace_id", "match": { "value": workspace_id.to_string() } },
                { "key": "bucket_name", "match": { "value": bucket_name } },
                { "key": "key", "match": { "value": key } },
            ],
        });
        self.delete(workspace_id, &json!({ "filter": filter }))
            .await
    }

    async fn delete(&self, workspace_id: WorkspaceId, body: &Value) -> Result<()> {
        let resp = self
            .http
            .post(format!(
                "{}/points/delete?wait=true",
                self.collection_url(workspace_id)
            ))
            .json(body)
            .send()
            .await?;
        if resp.status().as_u16() == 404 {
            return Ok(());
        }
        ok_or_api(resp).await.map(|_| ())
    }

    /// Drop a workspace's entire collection (full rebuild). A missing collection
    /// is a no-op.
    pub async fn delete_collection(&self, workspace_id: WorkspaceId) -> Result<()> {
        let resp = self
            .http
            .delete(self.collection_url(workspace_id))
            .send()
            .await?;
        if resp.status().as_u16() == 404 {
            return Ok(());
        }
        ok_or_api(resp).await.map(|_| ())
    }
}

fn parse_scored_point(hit: &Value) -> Result<ScoredPoint> {
    let id = hit
        .get("id")
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| VectorError::Malformed("hit missing string-uuid id".into()))?;
    let score = hit
        .get("score")
        .and_then(Value::as_f64)
        .ok_or_else(|| VectorError::Malformed("hit missing score".into()))? as f32;
    let payload = hit
        .get("payload")
        .ok_or_else(|| VectorError::Malformed("hit missing payload".into()))
        .and_then(PointPayload::from_qdrant)?;
    Ok(ScoredPoint { id, score, payload })
}

/// Turn a non-success response into a [`VectorError::Api`], else pass it through.
async fn ok_or_api(resp: reqwest::Response) -> Result<reqwest::Response> {
    if resp.status().is_success() {
        Ok(resp)
    } else {
        Err(api_error(resp).await)
    }
}

async fn api_error(resp: reqwest::Response) -> VectorError {
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    VectorError::Api { status, body }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_validates_url_and_builds_the_timed_client() {
        // A valid base builds (the timeout-configured client is constructed) and a
        // malformed one is rejected at parse time — same contract as before, now
        // over the default timed client rather than a bare `Client::new()`.
        assert!(VectorStore::new("http://localhost:6333").is_ok());
        assert!(VectorStore::new("not a url").is_err());
    }
}
