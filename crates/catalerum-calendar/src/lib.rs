//! catalerum-calendar — concrete [`CalendarProvider`] backends (SOUL §8).
//!
//! The trait itself lives in `catalerum-core`
//! ([`catalerum_core::CalendarProvider`]); this crate provides the
//! implementations and the factory that turns a [`Connection`] into a live
//! provider:
//!
//! - [`LocalIcsProvider`] — a directory of `.ics` files (one calendar per
//!   file). Read-only; content-hash cursor; idempotent re-sync. Optional
//!   best-effort filesystem watcher ([`watch::watch_dir`]).
//! - [`CalDavProvider`] — CalDAV (RFC 4791/6578 `sync-collection` REPORT +
//!   ETags) and `webcal://` (read-only ICS-over-HTTP `GET`).
//! - [`GoogleCalendarProvider`] — Google Calendar v3 (OAuth2 + `events.list`
//!   `syncToken` incremental sync). OAuth tokens are sealed behind the
//!   connection's `credential_ref` and reached through a [`GoogleTokenStore`]
//!   seam the ingest layer supplies, so building a Google provider needs that
//!   seam ([`provider_from_connection_with`]); the plain
//!   [`provider_from_connection`] (no seam) rejects a Google connection.
//!
//! All sync is incremental and idempotent by cursor / ETag / sync-token
//! (SOUL §3.4): re-running from the returned cursor never duplicates.
//!
//! # Mapping a [`Connection`] to a provider
//!
//! Core [`ConnectionKind`] is the abstract category `Calendar`; the *specific*
//! backend and its settings live in the connection's `config` JSON (mirroring
//! `catalerum-store`'s `connections.config` JSONB). The factory
//! [`provider_from_connection`] reads a `"provider"` discriminator
//! ([`CalendarSubKind`]) plus the backend's own keys:
//!
//! ```json
//! { "provider": "local",  "dir": "/var/calendars" }
//! { "provider": "caldav", "base_url": "https://dav.example.com/cal/work/",
//!   "username": "u", "password": "p" }
//! { "provider": "webcal", "base_url": "webcal://example.com/feed.ics" }
//! { "provider": "google",  "calendar": "primary" }
//! { "provider": "outlook", "calendar": "AAMk…" }  // absent ⇒ default calendar
//! ```
//!
//! The directory/URL keys are the canonical ones the API persists (`dir`,
//! `base_url`); the legacy aliases `path` / `url` are still accepted on read.
//!
//! When `"provider"` is absent the factory infers it from the other keys (a
//! `dir`/`path` ⇒ local, a `webcal://` URL ⇒ webcal, any other `base_url`/`url`
//! ⇒ caldav), so existing connections need no migration.

#![forbid(unsafe_code)]

pub mod caldav;
pub mod google;
pub mod ical;
pub mod local;
pub mod multistatus;
pub mod outlook;
pub mod watch;

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use catalerum_core::error::{Error, Result};
use catalerum_core::model::{Connection, ConnectionKind, Cursor};
use catalerum_core::provider::CalendarProvider;

pub use caldav::{CalDavMode, CalDavProvider};
pub use google::{
    exchange_code as google_exchange_code, GoogleCalendarProvider, GoogleTokenStore, GoogleTokens,
    WatchChannel as GoogleWatchChannel, AUTH_URL as GOOGLE_AUTH_URL,
    CALENDAR_EVENTS_SCOPE as GOOGLE_CALENDAR_EVENTS_SCOPE,
    CALENDAR_READONLY_SCOPE as GOOGLE_CALENDAR_READONLY_SCOPE,
};
pub use ical::{event_to_ics, parse_calendar, parse_vevents, ParsedEvent};
pub use local::LocalIcsProvider;
pub use multistatus::{parse_multistatus, MultiStatus, ResponseEntry};
pub use outlook::{
    auth_url as outlook_auth_url, exchange_code as outlook_exchange_code, OutlookCalendarProvider,
    OutlookTokenStore, OutlookTokens, OUTLOOK_CALENDAR_SCOPES,
};

/// The concrete calendar backend behind a [`Connection`] of kind
/// [`ConnectionKind::Calendar`]. Stored as the `"provider"` token in the
/// connection's `config` JSON (the abstract core `ConnectionKind` stays a
/// category, per SOUL §3.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CalendarSubKind {
    /// A local directory of `.ics` files ([`LocalIcsProvider`]).
    Local,
    /// A CalDAV collection ([`CalDavProvider`], read-write-capable).
    #[serde(rename = "caldav", alias = "cal_dav")]
    CalDav,
    /// A read-only `webcal://` / ICS-over-HTTP subscription ([`CalDavProvider`]
    /// in [`CalDavMode::Webcal`]).
    Webcal,
    /// Google Calendar over the v3 REST API ([`GoogleCalendarProvider`],
    /// read-write).
    Google,
    /// Outlook / Microsoft 365 over the Microsoft Graph API
    /// ([`OutlookCalendarProvider`], read-write). The `microsoft` alias is
    /// accepted on read.
    #[serde(rename = "outlook", alias = "microsoft")]
    Outlook,
}

/// The config key holding the [`CalendarSubKind`] discriminator.
pub const PROVIDER_KEY: &str = "provider";

impl CalendarSubKind {
    /// Read the sub-kind from a connection's `config`, falling back to
    /// inference from the present keys when `"provider"` is absent.
    pub fn from_config(config: &serde_json::Value) -> Result<Self> {
        if let Some(token) = config.get(PROVIDER_KEY).and_then(serde_json::Value::as_str) {
            return serde_json::from_value(serde_json::Value::String(token.to_string()))
                .map_err(|_| Error::invalid(format!("unknown calendar provider `{token}`")));
        }
        // Inference for configs that predate the explicit discriminator. A
        // local directory may be keyed by the canonical `dir` or the legacy
        // `path` alias (see [`local::DIR_CONFIG_KEYS`]).
        if local::DIR_CONFIG_KEYS
            .iter()
            .any(|key| config.get(*key).is_some())
        {
            return Ok(Self::Local);
        }
        if let Some(url) = caldav::config_keys::URL_KEYS
            .iter()
            .find_map(|key| config.get(*key).and_then(serde_json::Value::as_str))
        {
            let lower = url.trim().to_ascii_lowercase();
            return Ok(if lower.starts_with("webcal") || lower.ends_with(".ics") {
                Self::Webcal
            } else {
                Self::CalDav
            });
        }
        Err(Error::invalid(
            "calendar connection config has no `provider` and no recognisable keys",
        ))
    }
}

/// Build a live [`CalendarProvider`] from a [`Connection`] and its `config`
/// JSON (the same JSON `catalerum-store` persists in `connections.config`).
///
/// The connection must be of kind [`ConnectionKind::Calendar`]. The concrete
/// backend is chosen by [`CalendarSubKind::from_config`] (explicit `"provider"`
/// token, else inferred).
///
/// A **Google** connection needs the OAuth token seam — use
/// [`provider_from_connection_with`]; this entry (no seam) rejects it with a clear
/// [`Error`]. Local/CalDAV/webcal build unconditionally here.
///
/// The provider is returned boxed behind [`Arc`] so callers can store it
/// type-erased in the ingest scheduler.
pub fn provider_from_connection(
    connection: &Connection,
    config: &serde_json::Value,
) -> Result<Arc<dyn CalendarProvider>> {
    provider_from_connection_with(connection, config, None, None)
}

/// Like [`provider_from_connection`], but threads the OAuth token seams a
/// Google or Outlook connection needs (the ingest layer builds them backed by
/// the AES-GCM secret store, keyed by the connection's `credential_ref`). The
/// seams are ignored for other backends; a Google/Outlook connection whose
/// seam is `None` (no secret store configured, or no stored credential) errors
/// clearly.
pub fn provider_from_connection_with(
    connection: &Connection,
    config: &serde_json::Value,
    google_tokens: Option<Arc<dyn google::GoogleTokenStore>>,
    outlook_tokens: Option<Arc<dyn outlook::OutlookTokenStore>>,
) -> Result<Arc<dyn CalendarProvider>> {
    if connection.kind != ConnectionKind::Calendar {
        return Err(Error::invalid(format!(
            "connection {} is not a calendar connection (kind = {:?})",
            connection.id, connection.kind
        )));
    }

    let sub = CalendarSubKind::from_config(config)?;
    match sub {
        CalendarSubKind::Local => Ok(Arc::new(LocalIcsProvider::from_config(
            connection.workspace_id,
            connection.id,
            config,
        )?)),
        CalendarSubKind::CalDav | CalendarSubKind::Webcal => Ok(Arc::new(
            CalDavProvider::from_config(connection.workspace_id, connection.id, config)?,
        )),
        CalendarSubKind::Google => {
            let tokens = google_tokens.ok_or_else(|| {
                Error::invalid(
                    "Google Calendar connection has no OAuth credentials available — \
                     connect it via /auth/google/connect and set [secrets].master_key",
                )
            })?;
            Ok(Arc::new(GoogleCalendarProvider::from_config(
                connection.workspace_id,
                connection.id,
                config,
                tokens,
            )?))
        }
        CalendarSubKind::Outlook => {
            let tokens = outlook_tokens.ok_or_else(|| {
                Error::invalid(
                    "Outlook calendar connection has no OAuth credentials available — \
                     connect it via /auth/microsoft/connect and set [secrets].master_key",
                )
            })?;
            Ok(Arc::new(OutlookCalendarProvider::from_config(
                connection.workspace_id,
                connection.id,
                config,
                tokens,
            )?))
        }
    }
}

/// A content-hash [`Cursor`] (`sha256:<hex>`) over raw calendar bytes, used by the
/// local-`.ics` and CalDAV backends to detect "unchanged since last sync" without a
/// server-provided token. Shared so both backends hash identically.
pub(crate) fn content_cursor(bytes: &[u8]) -> Cursor {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Cursor::new(format!("sha256:{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use catalerum_core::id::{ConnectionId, WorkspaceId};

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
            CalendarSubKind::from_config(&serde_json::json!({"provider":"caldav"})).unwrap(),
            CalendarSubKind::CalDav
        );
        assert_eq!(
            CalendarSubKind::from_config(&serde_json::json!({"path":"/x"})).unwrap(),
            CalendarSubKind::Local
        );
        assert_eq!(
            CalendarSubKind::from_config(&serde_json::json!({"url":"webcal://h/f.ics"})).unwrap(),
            CalendarSubKind::Webcal
        );
        assert_eq!(
            CalendarSubKind::from_config(&serde_json::json!({"url":"https://h/cal/"})).unwrap(),
            CalendarSubKind::CalDav
        );
        assert!(CalendarSubKind::from_config(&serde_json::json!({"unknown":1})).is_err());
        assert!(CalendarSubKind::from_config(&serde_json::json!({"provider":"bogus"})).is_err());
    }

    #[test]
    fn factory_builds_local_and_caldav() {
        let c = conn(ConnectionKind::Calendar);
        let local =
            provider_from_connection(&c, &serde_json::json!({"provider":"local","path":"/tmp/c"}));
        assert!(local.is_ok());
        let dav = provider_from_connection(
            &c,
            &serde_json::json!({"provider":"caldav","url":"https://d/cal/"}),
        );
        assert!(dav.is_ok());
    }

    #[test]
    fn factory_builds_from_api_blessed_config() {
        // The exact config the API's `POST /connections` route persists:
        // `dir` for local, `base_url` for caldav/webcal, plus the stamped
        // `provider` token. The provider factory must accept it — this is the
        // API->ingest seam regression guard.
        let c = conn(ConnectionKind::Calendar);

        let local =
            provider_from_connection(&c, &serde_json::json!({"provider":"local","dir":"/tmp/m2"}));
        assert!(local.is_ok(), "API `dir` must build a local provider");

        let dav = provider_from_connection(
            &c,
            &serde_json::json!({"provider":"caldav","base_url":"https://d/cal/"}),
        );
        assert!(dav.is_ok(), "API `base_url` must build a caldav provider");

        let webcal = provider_from_connection(
            &c,
            &serde_json::json!({"provider":"webcal","base_url":"webcal://h/f.ics"}),
        );
        assert!(
            webcal.is_ok(),
            "API `base_url` must build a webcal provider"
        );
    }

    #[test]
    fn factory_rejects_non_calendar_connection() {
        let c = conn(ConnectionKind::Storage);
        let r = provider_from_connection(&c, &serde_json::json!({"provider":"local","path":"/x"}));
        assert!(matches!(r, Err(Error::Invalid(_))));
    }

    #[test]
    fn factory_rejects_google_without_token_seam() {
        // Without the OAuth token seam (no secret store / stored credential), a
        // Google connection can't build — a clear error, not a silent success.
        let c = conn(ConnectionKind::Calendar);
        let r = provider_from_connection(&c, &serde_json::json!({"provider":"google"}));
        assert!(matches!(r, Err(Error::Invalid(_))));
    }

    #[test]
    fn factory_builds_google_with_token_seam() {
        use async_trait::async_trait;
        use catalerum_core::error::Result as CoreResult;
        use google::{GoogleTokenStore, GoogleTokens};

        struct FakeStore;
        #[async_trait]
        impl GoogleTokenStore for FakeStore {
            async fn load(&self) -> CoreResult<GoogleTokens> {
                Ok(GoogleTokens {
                    client_id: "cid".into(),
                    client_secret: "sec".into(),
                    refresh_token: "rt".into(),
                    ..GoogleTokens::default()
                })
            }
            async fn store(&self, _tokens: &GoogleTokens) -> CoreResult<()> {
                Ok(())
            }
        }

        let c = conn(ConnectionKind::Calendar);
        let seam: Arc<dyn GoogleTokenStore> = Arc::new(FakeStore);
        let r = provider_from_connection_with(
            &c,
            &serde_json::json!({"provider":"google","calendar":"primary"}),
            Some(seam),
            None,
        );
        assert!(r.is_ok(), "google builds with the token seam present");
    }

    #[test]
    fn factory_rejects_outlook_without_token_seam_and_builds_with_it() {
        use async_trait::async_trait;
        use catalerum_core::error::Result as CoreResult;
        use outlook::{OutlookTokenStore, OutlookTokens};

        let c = conn(ConnectionKind::Calendar);
        // No seam ⇒ a clear error, not a silent success.
        let r = provider_from_connection(&c, &serde_json::json!({"provider":"outlook"}));
        assert!(matches!(r, Err(Error::Invalid(_))));
        // The `microsoft` alias resolves to the same sub-kind.
        assert_eq!(
            CalendarSubKind::from_config(&serde_json::json!({"provider":"microsoft"})).unwrap(),
            CalendarSubKind::Outlook
        );

        struct FakeStore;
        #[async_trait]
        impl OutlookTokenStore for FakeStore {
            async fn load(&self) -> CoreResult<OutlookTokens> {
                Ok(OutlookTokens {
                    client_id: "cid".into(),
                    client_secret: "sec".into(),
                    refresh_token: "rt".into(),
                    ..OutlookTokens::default()
                })
            }
            async fn store(&self, _tokens: &OutlookTokens) -> CoreResult<()> {
                Ok(())
            }
        }
        let seam: Arc<dyn OutlookTokenStore> = Arc::new(FakeStore);
        let r = provider_from_connection_with(
            &c,
            &serde_json::json!({"provider":"outlook"}),
            None,
            Some(seam),
        );
        assert!(r.is_ok(), "outlook builds with the token seam present");
    }
}
