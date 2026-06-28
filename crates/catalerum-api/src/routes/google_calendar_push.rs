//! Google Calendar **push** webhook (SOUL §8/§11/§16 M7 — push half).
//!
//! `POST /webhooks/google/calendar` — the **public, unauthenticated** endpoint
//! Google POSTs to when a watched calendar changes. Unlike the generic authed
//! webhook surface ([`crate::routes::webhooks`]), this one has no `Auth`
//! extractor: the request carries no session, only Google's channel headers, and
//! the `X-Goog-Channel-Token` (an HMAC-signed [`ChannelClaims`](crate::google_channel_link::ChannelClaims))
//! **is** its own authorization (§19), exactly like the public trigger-fire route.
//!
//! A notification carries only channel headers — not the change itself — so on a
//! verified `exists` (change) notification we trigger an incremental collect: one
//! immediate [`enqueue_collect_now`](catalerum_ingest::enqueue_collect_now) per
//! enabled collect automation on the token's connection (a connection with no such
//! automation gets nothing — the dormant-connection model). The initial `sync`
//! handshake is `200`-and-ignored; every verify failure collapses to an opaque
//! `404` (SOUL §18) so a probe learns nothing.
//!
//! **Burst debounce.** Google fans out a burst of notifications per change; a
//! short-TTL bus lock keyed by connection lets only the first through and acks the
//! rest `200` without re-enqueuing. Even an accepted duplicate would be harmless —
//! the collect committed-ledger (SOUL §29) makes a poll idempotent — the lock just
//! avoids a thundering herd across the fleet.

use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::Router;

use catalerum_automation::Trigger;
use catalerum_core::id::ConnectionId;
use catalerum_core::{Automation, WorkspaceId};

use crate::state::AppState;

/// How long the per-connection burst-debounce lock is held. Comfortably covers a
/// Google notification burst (they arrive within a second or two) without delaying
/// a genuinely-later change beyond it.
const DEBOUNCE_TTL: Duration = Duration::from_secs(10);

/// The Google header carrying our signed channel token (the channel's authorization).
const CHANNEL_TOKEN_HEADER: &str = "x-goog-channel-token";
/// The Google header naming the notification's resource state (`sync` | `exists` | …).
const RESOURCE_STATE_HEADER: &str = "x-goog-resource-state";

/// Mount the public Google Calendar push webhook.
pub fn router() -> Router<AppState> {
    Router::new().route("/webhooks/google/calendar", post(notify))
}

/// The resource state of a Google notification, reduced to what we act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResourceState {
    /// The initial handshake Google sends when a channel is created — no change yet.
    Sync,
    /// A real change (`exists` and any other non-`sync` state) — trigger a collect.
    Change,
}

/// Classify the `X-Goog-Resource-State` header. Only the literal `sync` handshake
/// is [`ResourceState::Sync`]; everything else (`exists`, an absent header, an
/// unknown future state) is treated as a [`ResourceState::Change`] so we never miss
/// a real change by over-narrowing.
fn classify_state(raw: Option<&str>) -> ResourceState {
    match raw.map(str::trim) {
        Some("sync") => ResourceState::Sync,
        _ => ResourceState::Change,
    }
}

async fn notify(State(state): State<AppState>, headers: HeaderMap) -> StatusCode {
    // The initial `sync` handshake carries no change — ack and ignore (no token
    // needed; a probe spamming `sync` learns nothing and triggers nothing).
    if classify_state(header_str(&headers, RESOURCE_STATE_HEADER)) == ResourceState::Sync {
        return StatusCode::OK;
    }

    // A change notification must carry a token that verifies; anything else → an
    // opaque 404 (forged / expired / missing all look identical to a probe).
    let Some(token) = header_str(&headers, CHANNEL_TOKEN_HEADER) else {
        return StatusCode::NOT_FOUND;
    };
    let now = chrono::Utc::now().timestamp();
    let Ok(claims) = state.google_channel_signer().verify(token, now) else {
        return StatusCode::NOT_FOUND;
    };

    // Fail closed + opaque on an archived workspace (mirror the trigger-fire redeem):
    // indistinguishable from a bad token.
    if workspace_archived(state.store(), claims.workspace_id).await {
        return StatusCode::NOT_FOUND;
    }

    // Cheap fleet-wide burst debounce (see module docs): first burst-mate proceeds,
    // the rest are acked without re-enqueuing. A lock-backend hiccup favors delivery.
    let debounce_key = format!("google-push:{}", claims.connection_id);
    match state
        .bus()
        .lock()
        .try_acquire(&debounce_key, DEBOUNCE_TTL)
        .await
    {
        Ok(Some(_guard)) => {} // first in the burst — proceed (left to TTL-expire)
        Ok(None) => return StatusCode::OK, // debounced duplicate
        Err(e) => tracing::debug!(error = %e, "google push debounce lock error; proceeding"),
    }

    enqueue_collect_for_connection(&state, claims.workspace_id, claims.connection_id).await;
    StatusCode::OK
}

/// Enqueue one immediate collect poll per enabled collect automation on
/// `connection_id`. A store hiccup is logged, not surfaced (we still ack `200`): a
/// dropped push just defers this change to the next poll cadence — push is a
/// latency optimization over the always-correct poll (SOUL §10/§28).
async fn enqueue_collect_for_connection(
    state: &AppState,
    workspace_id: WorkspaceId,
    connection_id: ConnectionId,
) {
    let automations = match state
        .store()
        .automations()
        .list_by_workspace(workspace_id)
        .await
    {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(error = %e, %workspace_id, "google push: listing automations failed; the poll cadence will still collect");
            return;
        }
    };
    let mut fired = 0usize;
    for automation in collect_automations_for(automations, connection_id) {
        match catalerum_ingest::enqueue_collect_now(state.store(), workspace_id, &automation).await
        {
            Ok(Some(_job)) => fired += 1,
            Ok(None) => {} // no (parseable) collect trigger after all — skip
            Err(e) => {
                tracing::warn!(error = %e, automation = %automation.id, "google push: enqueue collect failed")
            }
        }
    }
    if fired > 0 {
        tracing::debug!(%connection_id, fired, "google push: enqueued immediate collect(s)");
    }
}

/// The enabled collect automations that pull `connection_id` (SOUL §11). Pure — the
/// notification-fan-out test target. Filters to `enabled` automations whose collect
/// trigger names this exact connection (parsed as a [`ConnectionId`], so a `name`
/// or malformed connection never matches by accident).
fn collect_automations_for(
    automations: Vec<Automation>,
    connection_id: ConnectionId,
) -> Vec<Automation> {
    automations
        .into_iter()
        .filter(|a| a.enabled && automation_collects_connection(a, connection_id))
        .collect()
}

/// Whether any of `automation`'s triggers is a collect trigger that pulls
/// `connection_id`.
fn automation_collects_connection(automation: &Automation, connection_id: ConnectionId) -> bool {
    automation
        .triggers
        .iter()
        .filter_map(|t| serde_json::from_value::<Trigger>(t.clone()).ok())
        .filter_map(|tr| trigger_connection_id(&tr))
        .any(|id| id == connection_id)
}

/// The [`ConnectionId`] a collect trigger pulls from, or `None` for a non-collect
/// trigger or one whose `connection` isn't a valid id.
fn trigger_connection_id(trigger: &Trigger) -> Option<ConnectionId> {
    if !trigger.is_collect() {
        return None;
    }
    let raw = trigger.collect_connection()?;
    uuid::Uuid::parse_str(raw.trim())
        .ok()
        .map(ConnectionId::from_uuid)
}

/// Whether `ws` is an **archived** workspace (SOUL §18) — the public webhook fails
/// closed + opaque on it, exactly like a bad token. A live/vanished workspace is
/// not archived (a vanished one simply matches no automations).
async fn workspace_archived(store: &catalerum_store::Store, ws: WorkspaceId) -> bool {
    matches!(store.workspaces().get(ws).await, Ok(w) if w.archived_at.is_some())
}

/// Read a request header as a trimmed, non-empty `&str` (case-insensitive name).
fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn automation(enabled: bool, triggers: Vec<serde_json::Value>) -> Automation {
        Automation {
            id: catalerum_core::id::AutomationId::new(),
            workspace_id: WorkspaceId::new(),
            name: "a".into(),
            enabled,
            triggers,
            condition: None,
            actions: Vec::new(),
            spec: None,
            grant_id: None,
        }
    }

    #[test]
    fn classify_state_only_sync_is_the_handshake() {
        assert_eq!(classify_state(Some("sync")), ResourceState::Sync);
        assert_eq!(classify_state(Some(" sync ")), ResourceState::Sync);
        // Everything else (a real change, an unknown future state, absent) is a change.
        assert_eq!(classify_state(Some("exists")), ResourceState::Change);
        assert_eq!(classify_state(Some("not_exists")), ResourceState::Change);
        assert_eq!(classify_state(None), ResourceState::Change);
    }

    #[test]
    fn trigger_connection_id_parses_only_collect_triggers() {
        let conn = ConnectionId::new();
        let cc = serde_json::from_value::<Trigger>(json!({
            "kind": "collect_calendar", "connection": conn.to_string()
        }))
        .unwrap();
        assert_eq!(trigger_connection_id(&cc), Some(conn));
        // A non-collect trigger, or a non-id connection, yields None.
        let sched =
            serde_json::from_value::<Trigger>(json!({ "kind": "schedule", "cron": "* * * * *" }))
                .unwrap();
        assert_eq!(trigger_connection_id(&sched), None);
        let bad = serde_json::from_value::<Trigger>(json!({
            "kind": "collect_calendar", "connection": "not-a-uuid"
        }))
        .unwrap();
        assert_eq!(trigger_connection_id(&bad), None);
    }

    #[test]
    fn collect_automations_for_filters_enabled_and_matching_connection() {
        let target = ConnectionId::new();
        let other = ConnectionId::new();

        let matches = automation(
            true,
            vec![json!({ "kind": "collect_calendar", "connection": target.to_string() })],
        );
        let disabled = automation(
            false,
            vec![json!({ "kind": "collect_calendar", "connection": target.to_string() })],
        );
        let other_conn = automation(
            true,
            vec![json!({ "kind": "collect_calendar", "connection": other.to_string() })],
        );
        let not_collect = automation(
            true,
            vec![json!({ "kind": "schedule", "cron": "* * * * *" })],
        );

        let matches_id = matches.id;
        let kept =
            collect_automations_for(vec![matches, disabled, other_conn, not_collect], target);
        assert_eq!(
            kept.len(),
            1,
            "only the enabled, connection-matching collect automation"
        );
        assert_eq!(kept[0].id, matches_id);
    }

    /// The dedicated push route and the generic catch-all webhook route coexist in
    /// one router without a matchit conflict (static `/webhooks/google/calendar`
    /// takes precedence over `/webhooks/{*path}`). Merging is what triggers the
    /// route-table insert, so a conflict would panic here.
    #[test]
    fn push_route_coexists_with_generic_webhook_catch_all() {
        let _router: Router<AppState> = super::router().merge(crate::routes::webhooks::router());
    }
}
