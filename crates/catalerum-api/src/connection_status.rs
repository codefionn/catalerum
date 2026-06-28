//! Connection **collect status** (SOUL §29 — the *dormant-connection warning*).
//!
//! Registering an email or calendar [`Connection`] provisions **nothing**: it is
//! dormant until a user-authored automation headed by a `CollectEmail` /
//! `CollectCalendar` trigger (filled with that connection's id) polls it (SOUL
//! §10/§11/§28). A connection that no *enabled* Collect automation references will
//! never ingest anything — the "I added my email but see no mail" silent trap.
//!
//! This module annotates a listed connection with a single derived boolean,
//! `collecting`, so the connection-listing routes (`/email/connections`,
//! `/calendar/connections`) can hand the web UI enough to surface an inline
//! "idle" warning. It is a **pure, presentation-only** projection over the
//! workspace's automations — no mutation, no new endpoint. It also carries the
//! inverse guard, [`validate_collect_connections`]: at authoring time a collect
//! trigger must reference an existing connection of the matching kind, so a
//! placeholder connection string can never be persisted.
//!
//! The computation mirrors how the collect scanner resolves a source: a collect
//! trigger's `connection` field is a [`ConnectionId`] string
//! (`catalerum_ingest::collect::parse_connection` parses it), so a connection is
//! "collecting" iff some enabled automation carries a collect trigger **of the
//! matching kind** (`collect_email` for an [`Email`](ConnectionKind::Email)
//! connection, `collect_calendar` for a [`Calendar`](ConnectionKind::Calendar)
//! one) whose `connection` equals the connection's id. Matching the kind means a
//! stray `collect_calendar` pointed at an email connection never masks its
//! dormancy.

use serde::Serialize;

use catalerum_automation::Trigger;
use catalerum_core::model::{Connection, ConnectionKind};
use catalerum_core::{Automation, ConnectionId};

/// A [`Connection`] annotated with its collect status (SOUL §29). Serializes as
/// the connection's own fields (flattened) plus a top-level `collecting` boolean,
/// so the wire shape is the existing connection JSON with one additive field —
/// clients that ignore it keep working.
#[derive(Debug, Serialize)]
pub struct ConnectionView {
    /// The underlying connection, flattened onto the wire object.
    #[serde(flatten)]
    pub connection: Connection,
    /// `true` iff an **enabled** automation heads a matching Collect trigger at
    /// this connection. `false` ⇒ the connection is **dormant** (configured but
    /// nothing will ever ingest from it) — the UI's cue for an "idle" warning.
    /// Always `true` for a connection kind that has no Collect trigger
    /// (Storage/Channel/Postgres): dormancy is not a meaningful state there.
    pub collecting: bool,
}

/// The collect trigger `kind` that ingests a connection of `kind`, if any.
///
/// Only email and calendar connections are pulled by a Collect trigger (SOUL
/// §10/§28); the other connection kinds have no collect head, so they return
/// `None` and are never treated as dormant.
fn collect_kind_for(kind: ConnectionKind) -> Option<&'static str> {
    match kind {
        ConnectionKind::Email => Some("collect_email"),
        ConnectionKind::Calendar => Some("collect_calendar"),
        ConnectionKind::Storage | ConnectionKind::Channel | ConnectionKind::Postgres => None,
    }
}

/// Whether any **enabled** automation collects from the connection `id` of `kind`.
///
/// Scans `automations` in memory (the same `Vec<Automation>` the automations route
/// lists) for an enabled automation carrying a Collect trigger of the kind that
/// matches this connection kind whose `connection` field equals `id`. A disabled
/// automation never counts; a trigger spec that doesn't parse is skipped (it could
/// never have fired); a Collect trigger of the *wrong* kind (e.g. a
/// `collect_calendar` naming an email connection) never counts. A non-collectable
/// connection kind is reported as collecting (dormancy doesn't apply).
#[must_use]
pub fn is_collecting(automations: &[Automation], kind: ConnectionKind, id: ConnectionId) -> bool {
    let Some(want_kind) = collect_kind_for(kind) else {
        return true; // no Collect head for this kind ⇒ never "dormant"
    };
    let id = id.to_string();
    automations
        .iter()
        .filter(|a| a.enabled)
        .flat_map(|a| a.triggers.iter())
        .any(|spec| {
            let Ok(trigger) = serde_json::from_value::<Trigger>(spec.clone()) else {
                return false;
            };
            trigger.kind() == want_kind
                && trigger
                    .collect_connection()
                    .is_some_and(|conn| conn.trim() == id)
        })
}

/// Authoring-time guard (SOUL §10/§28): every collect trigger in `triggers` must
/// name an **existing** connection of the matching kind, by its id.
///
/// Before this guard, a placeholder like `"fastmail"` (or a copied
/// `"<connection-id>"` example) saved fine and then failed forever at poll time —
/// a doomed collect job re-enqueued every tick, visible only in the server log.
/// Failing the save instead gives the author (human or LLM) an actionable error
/// at the moment they can still fix it. Shared by the automation REST routes and
/// the `create_automation`/`update_automation` LLM tools, on the *compiled*
/// triggers so graph and legacy shapes validate identically. A trigger spec that
/// doesn't parse is skipped here — the shape validation before this guard already
/// rejects malformed collect triggers.
///
/// # Errors
/// A human-readable message naming the offending trigger and how to fix it.
pub(crate) async fn validate_collect_connections(
    store: &catalerum_store::Store,
    workspace_id: catalerum_core::WorkspaceId,
    triggers: &[serde_json::Value],
) -> Result<(), String> {
    for spec in triggers {
        let Ok(trigger) = serde_json::from_value::<Trigger>(spec.clone()) else {
            continue;
        };
        let Some(raw) = trigger.collect_connection() else {
            continue;
        };
        let (want, domain) = match trigger.kind() {
            "collect_email" => (ConnectionKind::Email, "email"),
            "collect_sql" => (ConnectionKind::Postgres, "postgres"),
            _ => (ConnectionKind::Calendar, "calendar"),
        };
        // How to obtain a connection of this kind, for the fix-it hints below
        // (there is no create_postgres_connection tool — external databases are
        // registered by an admin over REST).
        let create_hint = match want {
            ConnectionKind::Postgres => {
                "register the database first (an admin: POST /db/connections)".to_string()
            }
            _ => format!("create the connection first (in chat: create_{domain}_connection)"),
        };
        let raw = raw.trim();
        let id: ConnectionId = raw.parse().map_err(|_| {
            format!(
                "{} trigger: `connection` must be an existing {domain} connection's id \
                 (a uuid), got `{raw}` — {create_hint}, or list existing ones with \
                 list_connections (in the visual editor: configure the source on the \
                 collect node), then reference its id",
                trigger.kind()
            )
        })?;
        match store.connections().get(workspace_id, id).await {
            Ok(c) if c.kind == want => {}
            Ok(c) => {
                return Err(format!(
                    "{} trigger: connection `{raw}` is a {:?} connection, but this \
                     trigger collects {domain}",
                    trigger.kind(),
                    c.kind
                ))
            }
            Err(catalerum_store::StoreError::NotFound) => {
                return Err(format!(
                    "{} trigger: no {domain} connection with id `{raw}` exists in this \
                     workspace — list existing sources with list_connections, or \
                     {create_hint}, then reference its id",
                    trigger.kind()
                ))
            }
            Err(e) => return Err(format!("looking up connection `{raw}`: {e}")),
        }
    }
    Ok(())
}

/// Annotate each connection with its collect status (SOUL §29), preserving order.
#[must_use]
pub fn annotate(connections: Vec<Connection>, automations: &[Automation]) -> Vec<ConnectionView> {
    connections
        .into_iter()
        .map(|connection| {
            let collecting = is_collecting(automations, connection.kind, connection.id);
            ConnectionView {
                collecting,
                connection,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    use catalerum_core::model::Connection;
    use catalerum_core::{AutomationId, WorkspaceId};

    fn automation(enabled: bool, triggers: Vec<Value>) -> Automation {
        Automation {
            id: AutomationId::new(),
            workspace_id: WorkspaceId::new(),
            name: "auto".into(),
            enabled,
            triggers,
            condition: None,
            actions: vec![json!({ "kind": "write_email" })],
            spec: None,
            grant_id: None,
        }
    }

    fn connection(kind: ConnectionKind) -> Connection {
        Connection {
            id: ConnectionId::new(),
            workspace_id: WorkspaceId::new(),
            kind,
            name: "src".into(),
            credential_ref: None,
            cursor: None,
        }
    }

    #[test]
    fn enabled_matching_collect_email_marks_connection_collecting() {
        let conn = connection(ConnectionKind::Email);
        let autos = vec![automation(
            true,
            vec![json!({ "kind": "collect_email", "connection": conn.id.to_string() })],
        )];
        assert!(is_collecting(&autos, conn.kind, conn.id));
    }

    #[test]
    fn no_automation_means_dormant() {
        let conn = connection(ConnectionKind::Email);
        assert!(!is_collecting(&[], conn.kind, conn.id));
    }

    #[test]
    fn disabled_automation_does_not_count() {
        let conn = connection(ConnectionKind::Calendar);
        let autos = vec![automation(
            false,
            vec![json!({ "kind": "collect_calendar", "connection": conn.id.to_string() })],
        )];
        assert!(
            !is_collecting(&autos, conn.kind, conn.id),
            "a disabled collect automation leaves the connection dormant"
        );
    }

    #[test]
    fn non_matching_connection_id_stays_dormant() {
        let conn = connection(ConnectionKind::Email);
        // An enabled collect_email automation, but pointed at some *other* id.
        let autos = vec![automation(
            true,
            vec![json!({ "kind": "collect_email", "connection": ConnectionId::new().to_string() })],
        )];
        assert!(!is_collecting(&autos, conn.kind, conn.id));
    }

    #[test]
    fn kind_must_match_connection_kind() {
        // A collect_calendar trigger naming an *email* connection's id must not
        // mask the email connection's dormancy (email needs a collect_email).
        let conn = connection(ConnectionKind::Email);
        let autos = vec![automation(
            true,
            vec![json!({ "kind": "collect_calendar", "connection": conn.id.to_string() })],
        )];
        assert!(!is_collecting(&autos, conn.kind, conn.id));

        // The symmetric case: a calendar connection needs collect_calendar, and a
        // collect_email naming it does not count.
        let cal = connection(ConnectionKind::Calendar);
        let autos = vec![automation(
            true,
            vec![json!({ "kind": "collect_email", "connection": cal.id.to_string() })],
        )];
        assert!(!is_collecting(&autos, cal.kind, cal.id));
    }

    #[test]
    fn malformed_trigger_is_skipped_but_a_sibling_still_counts() {
        let conn = connection(ConnectionKind::Calendar);
        let autos = vec![automation(
            true,
            vec![
                json!({ "kind": "collect_calendar" }), // missing `connection` → won't parse
                json!({ "kind": "collect_calendar", "connection": conn.id.to_string() }),
            ],
        )];
        assert!(is_collecting(&autos, conn.kind, conn.id));
    }

    #[test]
    fn connection_id_is_trimmed_before_compare() {
        // The scanner trims the connection field; a padded id still matches.
        let conn = connection(ConnectionKind::Email);
        let padded = format!("  {}  ", conn.id);
        let autos = vec![automation(
            true,
            vec![json!({ "kind": "collect_email", "connection": padded })],
        )];
        assert!(is_collecting(&autos, conn.kind, conn.id));
    }

    #[test]
    fn non_collectable_kind_is_never_dormant() {
        for kind in [
            ConnectionKind::Storage,
            ConnectionKind::Channel,
            ConnectionKind::Postgres,
        ] {
            let conn = connection(kind);
            assert!(
                is_collecting(&[], conn.kind, conn.id),
                "{kind:?} has no Collect head, so it is never flagged dormant"
            );
        }
    }

    #[test]
    fn annotate_flattens_connection_and_sets_collecting() {
        let email = connection(ConnectionKind::Email);
        let email_id = email.id;
        let calendar = connection(ConnectionKind::Calendar); // no automation → dormant
        let autos = vec![automation(
            true,
            vec![json!({ "kind": "collect_email", "connection": email_id.to_string() })],
        )];

        let views = annotate(vec![email, calendar], &autos);
        assert_eq!(views.len(), 2);
        assert!(views[0].collecting, "the email source is collected");
        assert!(!views[1].collecting, "the calendar source is dormant");

        // The wire shape is the connection's fields plus a flat `collecting`.
        let wire = serde_json::to_value(&views[0]).unwrap();
        assert_eq!(wire["kind"], "email");
        assert_eq!(wire["id"], email_id.to_string());
        assert_eq!(wire["collecting"], true);
    }
}
