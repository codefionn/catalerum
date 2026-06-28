//! S3 (and S3-compatible, e.g. **MinIO**) [`StorageBackend`] (SOUL §9): blobs live
//! in an S3 bucket, addressed by object **key**; only catalogued metadata lands in
//! Postgres (the ingest layer's job, §10). The same `catalerum-core` trait the
//! local-filesystem backend implements, so the rest of the system is unchanged.
//!
//! Built for both AWS and self-hosted gateways: `endpoint` + `path_style` cover
//! MinIO (`http://host:9000`, path-style) while AWS uses the default regional host
//! (virtual-host style). Reads/writes buffer the whole object (mirrors the local
//! backend; multipart streaming is a later refinement).

use async_trait::async_trait;
use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use aws_sdk_s3::operation::get_object::GetObjectError;
use aws_sdk_s3::operation::head_object::HeadObjectError;
use aws_sdk_s3::primitives::{ByteStream as AwsByteStream, DateTime as AwsDateTime};
use aws_sdk_s3::Client;
use chrono::{DateTime, Utc};
use futures::stream::{self, BoxStream, StreamExt};

use catalerum_core::error::{Error, Result};
use catalerum_core::provider::{ByteStream, ObjectMeta, PutMeta, StorageBackend};

/// A [`StorageBackend`] over an S3 bucket (AWS or S3-compatible).
#[derive(Clone, Debug)]
pub struct S3Backend {
    client: Client,
    bucket: String,
    /// Retained for `ensure_container`'s `LocationConstraint` on real AWS.
    region: String,
}

impl S3Backend {
    /// Build a backend for `bucket` against an S3-compatible service. `endpoint` is
    /// the service URL (e.g. `http://localhost:9000` for MinIO; pass an empty string
    /// to use AWS's default regional endpoint). `path_style` must be **true** for
    /// MinIO and most self-hosted gateways (AWS prefers virtual-host style → false).
    #[must_use]
    pub fn new(
        endpoint: &str,
        region: &str,
        access_key: &str,
        secret_key: &str,
        bucket: impl Into<String>,
        path_style: bool,
    ) -> Self {
        let creds = Credentials::new(access_key, secret_key, None, None, "catalerum");
        let mut builder = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(region.to_string()))
            .credentials_provider(creds)
            .force_path_style(path_style);
        if !endpoint.trim().is_empty() {
            builder = builder.endpoint_url(endpoint);
        }
        Self {
            client: Client::from_conf(builder.build()),
            bucket: bucket.into(),
            region: region.to_string(),
        }
    }
}

#[async_trait]
impl StorageBackend for S3Backend {
    async fn list(&self, prefix: &str) -> Result<BoxStream<'static, Result<ObjectMeta>>> {
        let mut out: Vec<ObjectMeta> = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut req = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(prefix);
            if let Some(t) = &token {
                req = req.continuation_token(t);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| Error::provider(format!("s3 list: {e}")))?;
            for obj in resp.contents() {
                let Some(key) = obj.key() else { continue };
                out.push(ObjectMeta {
                    key: key.to_string(),
                    size: obj.size().unwrap_or(0).max(0) as u64,
                    etag: obj.e_tag().map(strip_quotes),
                    // `list` does not return per-object content types; `stat` does.
                    content_type: None,
                    last_modified: obj
                        .last_modified()
                        .and_then(to_chrono)
                        .unwrap_or_else(Utc::now),
                });
            }
            // Page until the listing is exhausted.
            if resp.is_truncated().unwrap_or(false) {
                token = resp.next_continuation_token().map(str::to_string);
                if token.is_none() {
                    break;
                }
            } else {
                break;
            }
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(stream::iter(out.into_iter().map(Ok)).boxed())
    }

    async fn stat(&self, key: &str) -> Result<ObjectMeta> {
        let resp = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| {
                if e.as_service_error()
                    .is_some_and(|s| matches!(s, HeadObjectError::NotFound(_)))
                {
                    Error::NotFound
                } else {
                    Error::provider(format!("s3 stat {key}: {e}"))
                }
            })?;
        Ok(ObjectMeta {
            key: key.to_string(),
            size: resp.content_length().unwrap_or(0).max(0) as u64,
            etag: resp.e_tag().map(strip_quotes),
            content_type: resp.content_type().map(str::to_string),
            last_modified: resp
                .last_modified()
                .and_then(to_chrono)
                .unwrap_or_else(Utc::now),
        })
    }

    async fn get(&self, key: &str) -> Result<ByteStream> {
        let resp = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| {
                // A missing key is `NoSuchKey`; a missing **bucket** is `NoSuchBucket`
                // — which `GetObjectError` does not model (→ `Unhandled`). Both are
                // 404s, so fall back to the HTTP status so neither becomes a 500.
                let not_found = e
                    .as_service_error()
                    .is_some_and(|s| matches!(s, GetObjectError::NoSuchKey(_)))
                    || e.raw_response().map(|r| r.status().as_u16()) == Some(404);
                if not_found {
                    Error::NotFound
                } else {
                    Error::provider(format!("s3 get {key}: {e}"))
                }
            })?;
        let data = resp
            .body
            .collect()
            .await
            .map_err(|e| Error::provider(format!("s3 get {key} body: {e}")))?;
        let bytes = data.into_bytes().to_vec();
        Ok(stream::once(async move { Ok(bytes) }).boxed())
    }

    async fn put(&self, key: &str, mut data: ByteStream, meta: PutMeta) -> Result<()> {
        // Buffer the stream (mirrors the local backend; multipart upload is later).
        let mut buf = Vec::new();
        while let Some(chunk) = data.next().await {
            buf.extend_from_slice(&chunk?);
        }
        let mut req = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(AwsByteStream::from(buf));
        if let Some(ct) = meta.content_type {
            req = req.content_type(ct);
        }
        req.send()
            .await
            .map_err(|e| Error::provider(format!("s3 put {key}: {e}")))?;
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<()> {
        // S3 `DeleteObject` is idempotent — an absent key returns success (204).
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| Error::provider(format!("s3 delete {key}: {e}")))?;
        Ok(())
    }

    /// Create the bucket if absent (idempotent — already-owned/exists is success).
    async fn ensure_container(&self) -> Result<()> {
        use aws_sdk_s3::operation::create_bucket::CreateBucketError;
        use aws_sdk_s3::types::{BucketLocationConstraint, CreateBucketConfiguration};
        let mut req = self.client.create_bucket().bucket(&self.bucket);
        // Real AWS requires a `LocationConstraint` for every region except
        // `us-east-1` (and rejects it being set *to* `us-east-1`); S3-compatible
        // gateways (MinIO) ignore it. Only attach it when needed.
        if self.region != "us-east-1" {
            req = req.create_bucket_configuration(
                CreateBucketConfiguration::builder()
                    .location_constraint(BucketLocationConstraint::from(self.region.as_str()))
                    .build(),
            );
        }
        match req.send().await {
            Ok(_) => Ok(()),
            Err(e)
                if e.as_service_error().is_some_and(|s| {
                    matches!(
                        s,
                        CreateBucketError::BucketAlreadyOwnedByYou(_)
                            | CreateBucketError::BucketAlreadyExists(_)
                    )
                }) =>
            {
                Ok(())
            }
            Err(e) => Err(Error::provider(format!(
                "s3 create bucket {}: {e}",
                self.bucket
            ))),
        }
    }
}

/// S3 ETags are quoted (`"abc123"`); store the bare value.
fn strip_quotes(etag: &str) -> String {
    etag.trim_matches('"').to_string()
}

/// Convert an AWS timestamp to a chrono UTC instant (`None` if out of range).
fn to_chrono(dt: &AwsDateTime) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp(dt.secs(), dt.subsec_nanos())
}
