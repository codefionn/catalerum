//! Google Calendar **push-channel** lifecycle worker (SOUL §8/§16 M7 — push half).
//!
//! Push is a latency optimization over the always-correct poll (SOUL §10/§28): a
//! Google `events.watch` channel makes Google POST to
//! `{[server].base_url}/webhooks/google/calendar` when a watched calendar changes,
//! so a change triggers a collect promptly instead of waiting for the poll cadence.
//! Channels **expire** (Google caps them at ~a week), so this worker ticks
//! periodically and, per Google-calendar connection:
//!
//! - **ensures** a live watch when the connection has ≥1 **enabled** collect
//!   automation and `[google].push` is on (creating one, or renewing one within the
//!   [`RENEW_LEAD_SECS`] window before it expires);
//! - **stops** the watch when no such automation remains (disabled / deleted) — the
//!   dormant-connection model: a connection nobody collects gets no channel.
//!
//! Opt-in and public-URL-gated: watching needs a publicly reachable **https**
//! `base_url` (Google refuses a plaintext or unreachable webhook), so the worker is
//! only spawned when `[google].push` is set and no-ops when the base URL isn't
//! https or the secret store (for OAuth tokens) is absent. Each connection's op is
//! single-fired across pods via the bus lock, exactly like the schedule scanners.
//!
//! The channel state (`{channel_id, resource_id, expiry}`) rides the connection's
//! `config.watch` JSON (see [`ConnectionRepo::set_watch_state`](catalerum_store::ConnectionRepo::set_watch_state))
//! — additive, no migration — read back here to renew/stop by the stored ids.

use std::collections::HashSet;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use catalerum_automation::Trigger;
use catalerum_core::id::ConnectionId;
use catalerum_core::model::Connection;
use catalerum_core::WorkspaceId;
use catalerum_ingest::{GoogleCalendarProvider, GoogleWatchChannel};

use crate::google_channel_link::ChannelClaims;
use crate::state::AppState;

/// How often the lifecycle scan runs. Channels live ~a week and renew a day early
/// ([`RENEW_LEAD_SECS`]), so an hourly pass renews with wide margin while staying
/// cheap (a bounded listing per workspace).
const WATCH_SCAN_TICK: Duration = Duration::from_secs(3600);
/// The channel lifetime requested from Google (its own max is ~1 week; the returned
/// expiry is authoritative).
const WATCH_TTL_SECS: i64 = 7 * 24 * 3600;
/// Renew a channel once it's within this of its expiry — a day of slack so an
/// hourly scan (or a brief outage) never lets one lapse.
const RENEW_LEAD_SECS: i64 = 24 * 3600;
/// The signed channel token is minted to outlive the channel by this much, so a
/// notification arriving right up to the channel's expiry still verifies.
const TOKEN_TTL_BUFFER_SECS: i64 = 24 * 3600;
/// TTL of the per-connection single-fire lock (so two pods don't both hit Google
/// for the same connection in one pass). Left to expire, never released.
const FIRE_LOCK_TTL: Duration = Duration::from_secs(300);

/// The `events.watch` webhook path — must match the route in
/// [`crate::routes::google_calendar_push`].
const WEBHOOK_PATH: &str = "/webhooks/google/calendar";

/// Whether a watch is due to be (re)created for a channel with the given `expiry`
/// (SOUL §8): a missing/unknown expiry is always due (never let a watch silently
/// lapse); otherwise due once `now` is within `lead_secs` of expiry. Pure — the
/// renewal-window test target.
fn watch_due(expiry: Option<DateTime<Utc>>, now: DateTime<Utc>, lead_secs: i64) -> bool {
    match expiry {
        None => true,
        Some(exp) => exp <= now + chrono::Duration::seconds(lead_secs),
    }
}

/// Whether an ensure pass must (re)create the watch: no channel yet, or the stored
/// one is due for renewal. Pure.
fn should_ensure(
    existing: Option<&GoogleWatchChannel>,
    now: DateTime<Utc>,
    lead_secs: i64,
) -> bool {
    match existing {
        None => true,
        Some(w) => watch_due(w.expiry, now, lead_secs),
    }
}

/// The set of connection ids pulled by **enabled** collect automations in a
/// workspace (SOUL §11) — the connections that "want" a live watch. Pure.
fn wanted_connection_ids(automations: &[catalerum_core::Automation]) -> HashSet<ConnectionId> {
    let mut ids = HashSet::new();
    for automation in automations.iter().filter(|a| a.enabled) {
        for trigger in &automation.triggers {
            if let Ok(t) = serde_json::from_value::<Trigger>(trigger.clone()) {
                if t.is_collect() {
                    if let Some(id) = t
                        .collect_connection()
                        .and_then(|c| uuid::Uuid::parse_str(c.trim()).ok())
                        .map(ConnectionId::from_uuid)
                    {
                        ids.insert(id);
                    }
                }
            }
        }
    }
    ids
}

/// Whether a connection's `config` is a Google calendar backend (the OAuth callback
/// always stamps `provider = "google"`). Kept a plain key check so the API needs no
/// direct `catalerum-calendar` dependency.
fn is_google_calendar(config: &serde_json::Value) -> bool {
    config.get("provider").and_then(serde_json::Value::as_str) == Some("google")
}

/// The push-channel lifecycle worker. Holds only [`AppState`] (like
/// [`StorageWatchWorker`](crate::StorageWatchWorker)); everything it needs — the
/// store, bus, secret store, `[google]` config, base URL, and channel signer — it
/// reads from state each pass.
pub struct GoogleWatchWorker {
    state: AppState,
    tick: Duration,
}

impl GoogleWatchWorker {
    /// Build the worker from app state with the default hourly scan tick.
    #[must_use]
    pub fn new(state: AppState) -> Self {
        Self {
            state,
            tick: WATCH_SCAN_TICK,
        }
    }

    /// Override the scan tick (tests use a short tick).
    #[must_use]
    pub fn with_tick(mut self, tick: Duration) -> Self {
        self.tick = tick;
        self
    }

    /// Spawn the scan loop as a detached background task.
    pub fn spawn(self) -> JoinHandle<()> {
        tokio::spawn(self.run())
    }

    async fn run(self) {
        info!(
            tick_secs = self.tick.as_secs(),
            "google watch worker started"
        );
        let mut ticker = tokio::time::interval(self.tick);
        // `interval`'s first tick fires immediately → an initial ensure pass at boot.
        loop {
            ticker.tick().await;
            self.scan().await;
        }
    }

    /// One lifecycle pass over every workspace's Google-calendar connections.
    async fn scan(&self) {
        let cfg = self.state.config();
        if !cfg.google.push || !cfg.google.is_enabled() {
            return; // push disabled — nothing to do
        }
        // Google only accepts an https webhook address; a plaintext/loopback base URL
        // can never work, so skip (with a hint) rather than spamming failed watches.
        let base = cfg.server.effective_base_url();
        if !base.starts_with("https://") {
            warn!(
                base_url = %base,
                "[google].push is on but [server].base_url is not https — Google requires an \
                 https webhook; no calendar watches will be created (poll cadence still runs)"
            );
            return;
        }
        // OAuth tokens live in the encrypted secret store; without it we can't auth
        // the events.watch call.
        if self.state.secret_store().is_none() {
            warn!(
                "[google].push is on but [secrets].master_key is unset — cannot authenticate \
                   events.watch; no calendar watches will be created"
            );
            return;
        }
        let address = format!("{base}{WEBHOOK_PATH}");

        let workspaces = match self.state.store().workspaces().list().await {
            Ok(w) => w,
            Err(e) => {
                warn!(error = %e, "google watch scan: listing workspaces failed; retry next tick");
                return;
            }
        };
        for ws in workspaces {
            self.scan_workspace(ws.id, &address).await;
        }
    }

    /// Ensure/stop watches for one workspace's Google-calendar connections.
    async fn scan_workspace(&self, ws: WorkspaceId, address: &str) {
        let store = self.state.store();
        let automations = match store.automations().list_by_workspace(ws).await {
            Ok(a) => a,
            Err(e) => {
                warn!(error = %e, %ws, "google watch scan: listing automations failed");
                return;
            }
        };
        let wanted = wanted_connection_ids(&automations);

        let connections = match store.connections().list_by_workspace(ws).await {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, %ws, "google watch scan: listing connections failed");
                return;
            }
        };
        for connection in connections {
            if connection.kind != catalerum_core::model::ConnectionKind::Calendar {
                continue;
            }
            self.reconcile_connection(ws, connection.id, wanted.contains(&connection.id), address)
                .await;
        }
    }

    /// Reconcile one connection: ensure a live watch when `wanted`, else stop any
    /// stored one. All Google I/O is best-effort — a failure is logged and retried
    /// next tick; the poll cadence remains the correctness path.
    async fn reconcile_connection(
        &self,
        ws: WorkspaceId,
        connection_id: ConnectionId,
        wanted: bool,
        address: &str,
    ) {
        let store = self.state.store();
        let row = match store.connections().get_row(ws, connection_id).await {
            Ok(r) => r,
            Err(e) => {
                debug!(error = %e, %connection_id, "google watch: connection vanished mid-scan");
                return;
            }
        };
        let config = row.config().clone();
        if !is_google_calendar(&config) {
            return; // not a Google calendar connection
        }
        let existing: Option<GoogleWatchChannel> = config
            .get("watch")
            .and_then(|v| serde_json::from_value(v.clone()).ok());

        let now = Utc::now();
        // Fast exit before any lock/HTTP: a wanted, still-fresh watch and an
        // unwanted, already-absent watch are both no-ops.
        if wanted && !should_ensure(existing.as_ref(), now, RENEW_LEAD_SECS) {
            return;
        }
        if !wanted && existing.is_none() {
            return;
        }

        // Single-fire this connection's op across pods (left to TTL-expire).
        let key = format!("google-watch:{connection_id}");
        match self
            .state
            .bus()
            .lock()
            .try_acquire(&key, FIRE_LOCK_TTL)
            .await
        {
            Ok(Some(_guard)) => {}
            Ok(None) => return, // another pod is handling this connection this pass
            Err(e) => {
                warn!(error = %e, %connection_id, "google watch: fire-lock error; skipping to avoid a double-op");
                return;
            }
        }

        // Build the provider (OAuth token seam from the encrypted secret store).
        let connection_dom: Connection = match row.clone().try_into() {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, %connection_id, "google watch: undecodable connection row");
                return;
            }
        };
        let Some(seam) = catalerum_ingest::google_token_store_for(
            self.state.secret_store(),
            &connection_dom,
            &config,
        ) else {
            debug!(%connection_id, "google watch: no OAuth seam (missing credential_ref?); skipping");
            return;
        };
        let provider = match GoogleCalendarProvider::from_config(ws, connection_id, &config, seam) {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, %connection_id, "google watch: building provider failed");
                return;
            }
        };

        if wanted {
            self.ensure_watch(ws, connection_id, &provider, address, existing, now)
                .await;
        } else if let Some(old) = existing {
            self.stop_and_clear(ws, connection_id, &provider, &old)
                .await;
        }
    }

    /// (Re)create a channel and persist it, then best-effort stop the old one. New
    /// first, old second, so there's never a gap in coverage.
    async fn ensure_watch(
        &self,
        ws: WorkspaceId,
        connection_id: ConnectionId,
        provider: &GoogleCalendarProvider,
        address: &str,
        existing: Option<GoogleWatchChannel>,
        now: DateTime<Utc>,
    ) {
        let channel_id = uuid::Uuid::new_v4().to_string();
        let exp = now.timestamp() + WATCH_TTL_SECS + TOKEN_TTL_BUFFER_SECS;
        let token = self.state.google_channel_signer().mint(&ChannelClaims {
            workspace_id: ws,
            connection_id,
            exp,
        });
        let channel = match provider
            .start_watch(address, &channel_id, &token, Some(WATCH_TTL_SECS))
            .await
        {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, %connection_id, "google watch: events.watch failed; keeping any existing watch, retry next tick");
                return;
            }
        };
        let value = match serde_json::to_value(&channel) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, %connection_id, "google watch: encoding channel state failed");
                return;
            }
        };
        if let Err(e) = self
            .state
            .store()
            .connections()
            .set_watch_state(ws, connection_id, Some(value))
            .await
        {
            warn!(error = %e, %connection_id, "google watch: persisting channel state failed");
            // Fall through: still try to stop any old channel below so we don't leak it.
        } else {
            debug!(%connection_id, expiry = ?channel.expiry, "google watch: channel (re)created");
        }
        // Stop the superseded channel, if any (and it isn't the one we just made).
        if let Some(old) = existing {
            if old.channel_id != channel.channel_id {
                if let Err(e) = provider.stop_watch(&old.channel_id, &old.resource_id).await {
                    debug!(error = %e, %connection_id, "google watch: stopping superseded channel failed (it will expire on its own)");
                }
            }
        }
    }

    /// Stop a channel (best-effort) and clear the stored state.
    async fn stop_and_clear(
        &self,
        ws: WorkspaceId,
        connection_id: ConnectionId,
        provider: &GoogleCalendarProvider,
        old: &GoogleWatchChannel,
    ) {
        if let Err(e) = provider.stop_watch(&old.channel_id, &old.resource_id).await {
            debug!(error = %e, %connection_id, "google watch: channels.stop failed (it will expire on its own)");
        }
        if let Err(e) = self
            .state
            .store()
            .connections()
            .set_watch_state(ws, connection_id, None)
            .await
        {
            warn!(error = %e, %connection_id, "google watch: clearing channel state failed");
        } else {
            debug!(%connection_id, "google watch: channel stopped (no collect automation)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ts(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).unwrap()
    }

    fn channel(expiry: Option<DateTime<Utc>>) -> GoogleWatchChannel {
        GoogleWatchChannel {
            channel_id: "c".into(),
            resource_id: "r".into(),
            expiry,
        }
    }

    #[test]
    fn watch_due_renews_only_within_the_lead_window() {
        let now = ts(1_000_000);
        let lead = RENEW_LEAD_SECS;
        // Expires well in the future ⇒ not due.
        assert!(!watch_due(
            Some(now + chrono::Duration::seconds(lead + 10)),
            now,
            lead
        ));
        // Exactly at the lead boundary ⇒ due (<=).
        assert!(watch_due(
            Some(now + chrono::Duration::seconds(lead)),
            now,
            lead
        ));
        // Already expired ⇒ due.
        assert!(watch_due(
            Some(now - chrono::Duration::seconds(1)),
            now,
            lead
        ));
        // Unknown expiry ⇒ always due (never let a watch silently lapse).
        assert!(watch_due(None, now, lead));
    }

    #[test]
    fn should_ensure_covers_absent_and_expiring() {
        let now = ts(1_000_000);
        // No channel at all ⇒ create.
        assert!(should_ensure(None, now, RENEW_LEAD_SECS));
        // Fresh channel ⇒ leave it.
        let fresh = channel(Some(now + chrono::Duration::seconds(RENEW_LEAD_SECS + 100)));
        assert!(!should_ensure(Some(&fresh), now, RENEW_LEAD_SECS));
        // Expiring channel ⇒ renew.
        let old = channel(Some(now + chrono::Duration::seconds(10)));
        assert!(should_ensure(Some(&old), now, RENEW_LEAD_SECS));
    }

    #[test]
    fn wanted_connection_ids_collects_enabled_collect_connections() {
        let a = ConnectionId::new();
        let b = ConnectionId::new();
        let mk = |enabled: bool, conn: &ConnectionId, kind: &str| catalerum_core::Automation {
            id: catalerum_core::id::AutomationId::new(),
            workspace_id: WorkspaceId::new(),
            name: "x".into(),
            enabled,
            triggers: vec![json!({ "kind": kind, "connection": conn.to_string() })],
            condition: None,
            actions: Vec::new(),
            spec: None,
            grant_id: None,
        };
        let automations = vec![
            mk(true, &a, "collect_calendar"),  // wanted
            mk(false, &b, "collect_calendar"), // disabled ⇒ ignored
            mk(true, &b, "webhook"),           // not a collect trigger ⇒ ignored
        ];
        let ids = wanted_connection_ids(&automations);
        assert!(ids.contains(&a));
        assert!(!ids.contains(&b));
        assert_eq!(ids.len(), 1);
    }

    #[test]
    fn is_google_calendar_matches_provider_key() {
        assert!(is_google_calendar(
            &json!({ "provider": "google", "calendar": "primary" })
        ));
        assert!(!is_google_calendar(&json!({ "provider": "caldav" })));
        assert!(!is_google_calendar(&json!({ "dir": "/x" })));
    }
}
