//! Build a live [`StorageBackend`] from a persisted storage [`Connection`] and
//! its `config` JSON (SOUL §9) — the storage twin of
//! [`catalerum_calendar::provider_from_connection`] /
//! [`catalerum_email::provider_from_connection`].
//!
//! A user-added storage backend (local folder / S3 / WebDAV) is persisted as a
//! `Connection` of kind [`ConnectionKind::Storage`] whose settings ride in the
//! `connections.config` JSON blob — exactly how calendar/email connections carry
//! their per-provider settings. This module reads that blob and constructs the
//! matching concrete backend, so a workspace can hold **many** backends (several
//! of the same kind) and a file can be stored to whichever one it chooses.
//!
//! **Secrets** (the S3 secret key, the WebDAV password) live in the config blob
//! verbatim today, matching the MCP-server precedent (`mcp_servers.auth`); a
//! follow-up moves them behind the §13 secret store. Callers that surface a
//! backend's config to a user MUST redact those fields first.

use std::sync::Arc;

use catalerum_core::error::{Error, Result};
use catalerum_core::model::{Connection, ConnectionKind};
use catalerum_core::provider::StorageBackend;
use serde_json::Value;

use crate::{LocalFsBackend, S3Backend, WebDavBackend};

/// The config key holding the [`StorageSubKind`] discriminator (`"kind"`), the
/// storage analogue of calendar's `"provider"`.
pub const KIND_KEY: &str = "kind";

/// Which concrete [`StorageBackend`] a storage [`Connection`] resolves to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageSubKind {
    /// A local-filesystem directory ([`LocalFsBackend`]).
    Local,
    /// An S3 / S3-compatible bucket ([`S3Backend`]).
    S3,
    /// A WebDAV collection ([`WebDavBackend`]).
    WebDav,
}

impl StorageSubKind {
    /// Read the sub-kind from a connection's `config`, falling back to inference
    /// from the present keys when the explicit `"kind"` discriminator is absent
    /// (mirrors [`catalerum_calendar`]'s lenient `from_config`).
    pub fn from_config(config: &Value) -> Result<Self> {
        if let Some(token) = config.get(KIND_KEY).and_then(Value::as_str) {
            return match token.trim().to_ascii_lowercase().as_str() {
                "local" | "local_fs" | "fs" | "folder" => Ok(Self::Local),
                "s3" => Ok(Self::S3),
                "webdav" | "dav" => Ok(Self::WebDav),
                other => Err(Error::invalid(format!("unknown storage kind `{other}`"))),
            };
        }
        // Inference for configs without an explicit discriminator.
        if config.get("access_key").is_some() || config.get("endpoint").is_some() {
            return Ok(Self::S3);
        }
        if str_field(config, &["url", "base_url"]).is_some() {
            return Ok(Self::WebDav);
        }
        if str_field(config, &["local_path", "path", "dir"]).is_some() {
            return Ok(Self::Local);
        }
        Err(Error::invalid(
            "storage connection config has no `kind` and no recognisable keys",
        ))
    }
}

/// Build a live [`StorageBackend`] from a [`Connection`] and its `config` JSON
/// (the same JSON `catalerum-store` persists in `connections.config`).
///
/// The connection must be of kind [`ConnectionKind::Storage`]. The concrete
/// backend is chosen by [`StorageSubKind::from_config`]. Secrets are read from
/// the blob verbatim (see the module docs). The backend is returned boxed behind
/// [`Arc`] so callers can store it type-erased in a registry.
pub fn backend_from_connection(
    connection: &Connection,
    config: &Value,
) -> Result<Arc<dyn StorageBackend>> {
    if connection.kind != ConnectionKind::Storage {
        return Err(Error::invalid(format!(
            "connection {} is not a storage connection (kind = {:?})",
            connection.id, connection.kind
        )));
    }
    backend_from_config(config)
}

/// Build a backend purely from a storage `config` blob (no `Connection` needed) —
/// the shared core of [`backend_from_connection`], also usable wherever only the
/// JSON settings are on hand.
pub fn backend_from_config(config: &Value) -> Result<Arc<dyn StorageBackend>> {
    match StorageSubKind::from_config(config)? {
        StorageSubKind::Local => {
            let path = str_field(config, &["local_path", "path", "dir"])
                .ok_or_else(|| Error::invalid("local storage config needs `local_path`"))?;
            Ok(Arc::new(LocalFsBackend::new(path.to_string())))
        }
        StorageSubKind::S3 => {
            let bucket = str_field(config, &["bucket"])
                .ok_or_else(|| Error::invalid("s3 storage config needs `bucket`"))?;
            let endpoint = str_field(config, &["endpoint"]).unwrap_or("");
            let region = {
                let r = str_field(config, &["region"]).unwrap_or("").trim();
                if r.is_empty() {
                    "us-east-1"
                } else {
                    r
                }
            };
            let access_key = str_field(config, &["access_key"]).unwrap_or("");
            let secret_key = str_field(config, &["secret_key"]).unwrap_or("");
            let path_style = config
                .get("path_style")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Ok(Arc::new(S3Backend::new(
                endpoint,
                region,
                access_key,
                secret_key,
                bucket.to_string(),
                path_style,
            )))
        }
        StorageSubKind::WebDav => {
            let url = str_field(config, &["url", "base_url"])
                .ok_or_else(|| Error::invalid("webdav storage config needs `url`"))?;
            let username = str_field(config, &["username"]).unwrap_or("");
            let password = str_field(config, &["password"]).unwrap_or("");
            Ok(Arc::new(WebDavBackend::new(url, username, password)?))
        }
    }
}

/// First present, string-typed value among `keys` in `config`.
fn str_field<'a>(config: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| config.get(*k).and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use catalerum_core::id::{ConnectionId, WorkspaceId};
    use serde_json::json;

    fn conn(kind: ConnectionKind) -> Connection {
        Connection {
            id: ConnectionId::new(),
            workspace_id: WorkspaceId::new(),
            kind,
            name: "test".into(),
            credential_ref: None,
            cursor: None,
        }
    }

    #[test]
    fn sub_kind_explicit_and_inferred() {
        assert_eq!(
            StorageSubKind::from_config(&json!({"kind":"s3"})).unwrap(),
            StorageSubKind::S3
        );
        assert_eq!(
            StorageSubKind::from_config(&json!({"kind":"webdav"})).unwrap(),
            StorageSubKind::WebDav
        );
        assert_eq!(
            StorageSubKind::from_config(&json!({"kind":"local"})).unwrap(),
            StorageSubKind::Local
        );
        // Inference when `kind` is absent.
        assert_eq!(
            StorageSubKind::from_config(&json!({"local_path":"/data"})).unwrap(),
            StorageSubKind::Local
        );
        assert_eq!(
            StorageSubKind::from_config(&json!({"endpoint":"http://m:9000","access_key":"k"}))
                .unwrap(),
            StorageSubKind::S3
        );
        assert_eq!(
            StorageSubKind::from_config(&json!({"url":"http://dav/"})).unwrap(),
            StorageSubKind::WebDav
        );
        // Unknown discriminator + empty config are rejected.
        assert!(StorageSubKind::from_config(&json!({"kind":"floppy"})).is_err());
        assert!(StorageSubKind::from_config(&json!({})).is_err());
    }

    #[test]
    fn rejects_non_storage_connection() {
        // `Arc<dyn StorageBackend>` is not `Debug`, so match the error directly
        // rather than `unwrap_err()`.
        let result = backend_from_connection(
            &conn(ConnectionKind::Calendar),
            &json!({"kind":"local","local_path":"/x"}),
        );
        assert!(matches!(result, Err(Error::Invalid(_))));
    }

    #[test]
    fn builds_local_and_validates_required_fields() {
        // A local backend builds from a path.
        backend_from_connection(
            &conn(ConnectionKind::Storage),
            &json!({"kind":"local","local_path":"/tmp/x"}),
        )
        .expect("local backend builds");
        // S3 without a bucket is rejected (its container has no name).
        assert!(backend_from_config(&json!({"kind":"s3","endpoint":"http://m:9000"})).is_err());
        // WebDAV without a url is rejected.
        assert!(backend_from_config(&json!({"kind":"webdav"})).is_err());
    }
}
