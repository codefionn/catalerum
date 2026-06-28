//! catalerum-storage — concrete [`StorageBackend`](catalerum_core::provider::StorageBackend)
//! impls (SOUL §9). Blobs stay in buckets; only catalogued metadata lands in
//! Postgres.
//!
//! This slice ships the **local-filesystem** ([`LocalFsBackend`]), **S3 /
//! S3-compatible** ([`S3Backend`], e.g. MinIO), and **WebDAV** ([`WebDavBackend`],
//! e.g. Nextcloud / `rclone serve webdav`) backends — all behind the same
//! `catalerum-core` trait (an optional `MultiBackend` could layer on later).

pub mod connection;
pub mod local;
pub mod s3;
pub mod sync;
pub mod webdav;

pub use connection::{backend_from_config, backend_from_connection, StorageSubKind};
pub use local::LocalFsBackend;
pub use s3::S3Backend;
pub use sync::sync_dir_to_backend;
pub use webdav::WebDavBackend;
