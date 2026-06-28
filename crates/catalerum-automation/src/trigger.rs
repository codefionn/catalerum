//! Trigger matching (SOUL §11) — the front half of dispatch: given a real-world
//! [`TriggerEvent`], decide which of a workspace's automations should fire.
//!
//! This covers the **push-driven** triggers (a Kanban task moved §24, an inbound
//! webhook, a channel message §25, a storage-object event §9, an explicit named
//! signal fired from inside the app §12). The **poll/time**
//! triggers (`Schedule`, `GraphQuery`, and a `CalendarEvent` lead) never match an
//! ad-hoc event — a scheduler evaluates *those* on a clock, a later slice. The
//! *delivery* half (durable Valkey-Streams fan-out, single-fire locking, the §6.2
//! reconciler) also lands later; this module is the pure matching predicate.

use serde::{Deserialize, Serialize};

use catalerum_core::Automation;

use crate::Trigger;

/// A real-world event that can fire push-driven triggers (SOUL §11). Mirrors the
/// matchable [`Trigger`] kinds; the dispatch layer builds one of these from a
/// Kanban move, a webhook hit, an inbound channel message, or a storage event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TriggerEvent {
    /// A Kanban task moved into `to_column` on `board` (§24).
    TaskMoved { board: String, to_column: String },
    /// An inbound webhook at `path`.
    Webhook { path: String },
    /// An inbound message on `channel` (§25). `text` is the message body — carried
    /// on the run's trigger (so an `LlmAgent` action sees what was said) **and**
    /// matched against the trigger's optional `{"text": …}` case-insensitive
    /// substring filter; with no such filter the trigger matches by `channel` alone.
    /// `sender` is the provider-native id of who spoke (a Matrix `@user:hs`, a
    /// Telegram user id) — carried for the agent so it knows *which* participant in a
    /// multi-party room sent this (multiplayer); it is not part of matching.
    ChannelMessage {
        channel: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sender: Option<String>,
    },
    /// A storage-object `event` (`created` / `updated` / `deleted`) on `key` within
    /// `bucket` (§9). `content_type` is the object's guessed MIME (when known) —
    /// carried on the run's trigger so a matching automation's downstream nodes can
    /// key on the file type (e.g. branch on an office document); it is not part of
    /// matching, which keys on the key's extension via the trigger's `extensions`.
    StorageObject {
        event: String,
        bucket: String,
        key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content_type: Option<String>,
    },
    /// An explicit **named signal** fired from inside catalerum (§11/§12) — most
    /// often an emerged UI handler calling the `fire_trigger` tool, but equally a
    /// chat/code-node caller. `name` is the signal name matched (exact) against a
    /// `{ "kind": "trigger", "name": … }` trigger; `payload` is optional
    /// caller-supplied context carried on the run's trigger for downstream nodes /
    /// an `LlmAgent` to read — it is **not** part of matching.
    Trigger {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload: Option<serde_json::Value>,
    },
}

impl Trigger {
    /// Whether this trigger fires for `event`. Only the push-driven kinds can
    /// match; a `Schedule`/`GraphQuery`/`CalendarEvent`/`CollectEmail`/
    /// `CollectCalendar` trigger always returns `false` here (a scheduler/the
    /// collect scanner drives those on a cadence). Each kind keys on its decisive
    /// field(s): webhook on `path`; channel on `channel` plus an optional
    /// `{"text": …}` case-insensitive substring filter on the message body; storage
    /// on `event` + `bucket` + a key `prefix`; a manual `Trigger` on `name` (exact).
    /// The `CalendarEvent` `lead`/`filter` predicates stay opaque (their scheduler
    /// isn't here yet).
    #[must_use]
    pub fn matches(&self, event: &TriggerEvent) -> bool {
        match (self, event) {
            (
                Trigger::TaskMoved { board, to_column },
                TriggerEvent::TaskMoved {
                    board: eb,
                    to_column: ec,
                },
            ) => board == eb && to_column == ec,
            (Trigger::Webhook { path }, TriggerEvent::Webhook { path: ep }) => path == ep,
            (Trigger::Trigger { name }, TriggerEvent::Trigger { name: en, .. }) => name == en,
            (
                Trigger::ChannelMessage { channel, filter },
                TriggerEvent::ChannelMessage {
                    channel: ec, text, ..
                },
            ) => {
                channel == ec && contains_ci(channel_text_filter(filter.as_ref()), text.as_deref())
            }
            (
                Trigger::StorageObject {
                    event,
                    bucket,
                    prefix,
                    extensions,
                },
                TriggerEvent::StorageObject {
                    event: ee,
                    bucket: eb,
                    key,
                    ..
                },
            ) => {
                event == ee
                    && bucket == eb
                    && prefix.as_ref().is_none_or(|p| key.starts_with(p.as_str()))
                    && key_has_extension(key, extensions)
            }
            _ => false,
        }
    }
}

/// Extract an optional case-insensitive **text** substring requirement from a
/// `ChannelMessage` trigger's opaque `filter` predicate. The interim convention
/// is an object with a string `"text"` key (e.g. `{"text": "deploy"}` ⟹ "fire
/// only on messages whose body contains 'deploy'"). Any other shape — absent,
/// non-object, or no string `text` — expresses no text constraint, so the trigger
/// keeps matching on `channel` alone; this stays backward-compatible with filters
/// written while the field was inert, and a richer predicate language (§11) can
/// supersede it later.
fn channel_text_filter(filter: Option<&serde_json::Value>) -> Option<&str> {
    filter?.get("text")?.as_str()
}

/// Whether `key`'s file extension is in a `StorageObject` trigger's `extensions`
/// allow-list. An **empty** list imposes no constraint (every key matches) — so a
/// trigger authored without `extensions` keeps its bucket/prefix-only behaviour.
///
/// The extension is the text after the last dot **in the file name** (the segment
/// past the final `/`): `inbox/report.docx` ⇒ `docx`, `a.b.tar.gz` ⇒ `gz`,
/// `my.dir/file` ⇒ *no extension* (the dot is in a parent segment). A key with no
/// extension never satisfies a non-empty list. Comparison is ASCII-case-insensitive
/// and each allow-list entry's leading dot is optional, so `["docx"]`, `[".docx"]`,
/// and `["DOCX"]` all match `report.docx`.
fn key_has_extension(key: &str, extensions: &[String]) -> bool {
    if extensions.is_empty() {
        return true;
    }
    let name = key.rsplit('/').next().unwrap_or(key);
    let ext = match name.rsplit_once('.') {
        Some((_, ext)) if !ext.is_empty() => ext,
        _ => return false,
    };
    extensions
        .iter()
        .any(|want| want.trim_start_matches('.').eq_ignore_ascii_case(ext))
}

/// A case-insensitive substring filter: `None` filter matches anything; a
/// `Some(f)` filter requires the candidate to be present and contain `f`.
///
/// Case folding is **Unicode-aware** (`to_lowercase`), not ASCII-only, so a filter
/// on non-English text matches regardless of case — e.g. `über` matches `Über`,
/// `café` matches `CAFÉ` — which ASCII folding (leaving accented/non-Latin letters
/// untouched) would miss. Channel-message and calendar-event filters both route
/// through here, so both gain it.
fn contains_ci(filter: Option<&str>, candidate: Option<&str>) -> bool {
    match filter {
        None => true,
        Some(f) => candidate.is_some_and(|c| c.to_lowercase().contains(&f.to_lowercase())),
    }
}

/// Whether a `CalendarEvent` trigger's opaque `filter` admits an event with the
/// given `summary`, `location`, and `description` (SOUL §8/§11). Unlike the
/// push-driven [`Trigger::matches`] kinds, a `CalendarEvent` trigger is fired by
/// the *scheduler* on a lead instant, not by an ad-hoc event — so this predicate
/// is evaluated there (`catalerum-ingest`'s `scan_calendar_event_triggers`)
/// rather than in `matches`, but it follows the same interim-convention spirit.
///
/// The convention mirrors the collect triggers' `filter` shape: `filter` is an
/// object with optional **string** keys `"summary"` / `"location"` /
/// `"description"`, each a case-insensitive substring the corresponding event
/// field must contain; all supplied keys **AND** together (so
/// `{"summary":"standup","location":"zoom"}` fires only on a Zoom standup). Any
/// other shape — `None`, a non-object, or no recognised string key — expresses
/// **no** constraint, so every event in the lead window fires; this stays
/// backward-compatible with `filter`s authored while the field was inert, and a
/// richer predicate language (§11) can supersede it later.
///
/// `summary` is always present on an event; `location`/`description` are optional,
/// and (per [`contains_ci`]) a filter on an absent field never matches — "fire on
/// events whose location mentions X" correctly skips events that have no location.
#[must_use]
pub fn calendar_event_filter_matches(
    filter: Option<&serde_json::Value>,
    summary: &str,
    location: Option<&str>,
    description: Option<&str>,
) -> bool {
    let Some(obj) = filter.and_then(serde_json::Value::as_object) else {
        return true; // absent / non-object filter ⇒ no constraint
    };
    let want = |key: &str| obj.get(key).and_then(serde_json::Value::as_str);
    contains_ci(want("summary"), Some(summary))
        && contains_ci(want("location"), location)
        && contains_ci(want("description"), description)
}

/// Does `automation` have an enabled, matching trigger for `event`? A disabled
/// automation never matches; a trigger spec that doesn't parse is skipped (it can
/// never have been valid to fire).
#[must_use]
pub fn automation_matches(automation: &Automation, event: &TriggerEvent) -> bool {
    automation.enabled
        && automation.triggers.iter().any(|t| {
            serde_json::from_value::<Trigger>(t.clone())
                .map(|trigger| trigger.matches(event))
                .unwrap_or(false)
        })
}

/// The enabled automations whose triggers match `event` — the candidates the
/// dispatch layer would run for it (SOUL §11). Order is preserved from the input.
#[must_use]
pub fn matching_automations<'a>(
    automations: &'a [Automation],
    event: &TriggerEvent,
) -> Vec<&'a Automation> {
    automations
        .iter()
        .filter(|a| automation_matches(a, event))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn trigger(v: Value) -> Trigger {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn task_moved_matches_on_board_and_column() {
        let t = trigger(json!({ "kind": "task_moved", "board": "sprint", "to_column": "done" }));
        assert!(t.matches(&TriggerEvent::TaskMoved {
            board: "sprint".into(),
            to_column: "done".into()
        }));
        // Wrong column or board → no match.
        assert!(!t.matches(&TriggerEvent::TaskMoved {
            board: "sprint".into(),
            to_column: "doing".into()
        }));
        assert!(!t.matches(&TriggerEvent::TaskMoved {
            board: "other".into(),
            to_column: "done".into()
        }));
        // Different event kind → no match.
        assert!(!t.matches(&TriggerEvent::Webhook { path: "/x".into() }));
    }

    #[test]
    fn webhook_and_channel_match_on_their_key() {
        assert!(
            trigger(json!({ "kind": "webhook", "path": "/hook" })).matches(
                &TriggerEvent::Webhook {
                    path: "/hook".into()
                }
            )
        );
        assert!(
            !trigger(json!({ "kind": "webhook", "path": "/hook" })).matches(
                &TriggerEvent::Webhook {
                    path: "/other".into()
                }
            )
        );
        // A filter with no `text` key expresses no body constraint, so channel is
        // decisive: it fires on the right channel and not on a wrong one — and is
        // backward-compatible with filters written while the field was inert.
        let ch =
            trigger(json!({ "kind": "channel_message", "channel": "ops", "filter": {"x": 1} }));
        assert!(ch.matches(&TriggerEvent::ChannelMessage {
            channel: "ops".into(),
            text: Some("hi".into()),
            sender: None,
        }));
        // Wrong channel never matches.
        assert!(!ch.matches(&TriggerEvent::ChannelMessage {
            channel: "other".into(),
            text: None,
            sender: None,
        }));
        assert!(
            !ch.matches(&TriggerEvent::Webhook {
                path: "/ops".into()
            }),
            "wrong kind never matches"
        );
    }

    #[test]
    fn manual_trigger_matches_on_name_and_ignores_payload() {
        // A named signal fires an automation whose `trigger` names the same signal;
        // the optional payload is carried, never matched.
        let t = trigger(json!({ "kind": "trigger", "name": "refresh" }));
        assert!(t.matches(&TriggerEvent::Trigger {
            name: "refresh".into(),
            payload: Some(json!({ "row": 3 })),
        }));
        // A different name never matches (exact, case-sensitive).
        assert!(!t.matches(&TriggerEvent::Trigger {
            name: "Refresh".into(),
            payload: None,
        }));
        assert!(!t.matches(&TriggerEvent::Trigger {
            name: "other".into(),
            payload: None,
        }));
        // A different event kind never matches.
        assert!(!t.matches(&TriggerEvent::Webhook {
            path: "/refresh".into()
        }));
        assert_eq!(t.kind(), "trigger");
    }

    #[test]
    fn channel_message_text_filter_matches_body_case_insensitively() {
        // A `{"text": …}` filter requires the message body to contain it (CI).
        let t = trigger(
            json!({ "kind": "channel_message", "channel": "ops", "filter": {"text": "DEPLOY"} }),
        );
        assert!(t.matches(&TriggerEvent::ChannelMessage {
            channel: "ops".into(),
            text: Some("please deploy now".into()),
            sender: None,
        }));
        // Right channel but the body lacks the substring → no match.
        assert!(!t.matches(&TriggerEvent::ChannelMessage {
            channel: "ops".into(),
            text: Some("nothing to do".into()),
            sender: None,
        }));
        // A text filter requires a present body.
        assert!(!t.matches(&TriggerEvent::ChannelMessage {
            channel: "ops".into(),
            text: None,
            sender: None,
        }));
        // The channel is still decisive: a matching body on the wrong channel fails.
        assert!(!t.matches(&TriggerEvent::ChannelMessage {
            channel: "other".into(),
            text: Some("deploy".into()),
            sender: None,
        }));

        // The helper reads only a string `text`; other shapes impose no constraint.
        assert_eq!(channel_text_filter(Some(&json!({"text": "x"}))), Some("x"));
        assert_eq!(channel_text_filter(Some(&json!({"text": 5}))), None);
        assert_eq!(channel_text_filter(Some(&json!({"other": "x"}))), None);
        assert_eq!(channel_text_filter(Some(&json!("x"))), None);
        assert_eq!(channel_text_filter(None), None);
    }

    #[test]
    fn text_filter_case_folding_is_unicode_aware() {
        // Non-ASCII case differences match (ASCII-only folding would miss these).
        let t = trigger(
            json!({ "kind": "channel_message", "channel": "ops", "filter": {"text": "über"} }),
        );
        assert!(t.matches(&TriggerEvent::ChannelMessage {
            channel: "ops".into(),
            text: Some("Bitte ÜBER prüfen".into()),
            sender: None,
        }));
        // And the helper directly: accented + non-Latin upper/lower fold.
        assert!(contains_ci(
            Some("café"),
            Some("Reserved a table at the CAFÉ")
        ));
        assert!(contains_ci(Some("ΣΊΓΜΑ"), Some("the σίγμα value")));
        // A genuine non-match still fails.
        assert!(!contains_ci(Some("über"), Some("nothing here")));
    }

    #[test]
    fn calendar_event_filter_matches_summary_location_description() {
        // No / non-object filter ⇒ no constraint: every event fires (back-compat
        // with the inert-field era).
        assert!(calendar_event_filter_matches(None, "Standup", None, None));
        assert!(calendar_event_filter_matches(
            Some(&json!("nope")),
            "Standup",
            None,
            None
        ));
        // A `{"summary": …}` filter requires the summary to contain it (CI).
        let f = json!({ "summary": "STANDUP" });
        assert!(calendar_event_filter_matches(
            Some(&f),
            "Daily standup",
            None,
            None
        ));
        assert!(!calendar_event_filter_matches(
            Some(&f),
            "Lunch",
            None,
            None
        ));
        // location/description filters key on the optional fields; an absent field
        // never satisfies a filter that names it.
        let loc = json!({ "location": "zoom" });
        assert!(calendar_event_filter_matches(
            Some(&loc),
            "Sync",
            Some("Zoom call"),
            None
        ));
        assert!(!calendar_event_filter_matches(
            Some(&loc),
            "Sync",
            None,
            None
        ));
        let desc = json!({ "description": "agenda" });
        assert!(calendar_event_filter_matches(
            Some(&desc),
            "Sync",
            None,
            Some("See the AGENDA below")
        ));
        // All supplied keys AND together: a Zoom standup matches; a Zoom lunch does
        // not (summary conjunct fails).
        let both = json!({ "summary": "standup", "location": "zoom" });
        assert!(calendar_event_filter_matches(
            Some(&both),
            "Team standup",
            Some("zoom.us/123"),
            None
        ));
        assert!(!calendar_event_filter_matches(
            Some(&both),
            "Team lunch",
            Some("zoom.us/123"),
            None
        ));
        // A non-string filter value imposes no constraint for that key (lenient,
        // like `channel_text_filter`).
        assert!(calendar_event_filter_matches(
            Some(&json!({ "summary": 5 })),
            "anything",
            None,
            None
        ));
    }

    /// A `StorageObject` event (content type omitted — not part of matching).
    fn storage_ev(event: &str, bucket: &str, key: &str) -> TriggerEvent {
        TriggerEvent::StorageObject {
            event: event.into(),
            bucket: bucket.into(),
            key: key.into(),
            content_type: None,
        }
    }

    #[test]
    fn storage_object_matches_event_bucket_and_key_prefix() {
        let t = trigger(
            json!({ "kind": "storage_object", "event": "created", "bucket": "docs", "prefix": "inbox/" }),
        );
        assert!(t.matches(&storage_ev("created", "docs", "inbox/report.pdf")));
        // Key outside the prefix → no match; wrong event/bucket → no match.
        assert!(!t.matches(&storage_ev("created", "docs", "archive/old.pdf")));
        assert!(!t.matches(&storage_ev("deleted", "docs", "inbox/x")));
        // A different bucket (same event + valid prefix) → no match — isolates the
        // `bucket` conjunct.
        assert!(!t.matches(&storage_ev("created", "other", "inbox/report.pdf")));
        // A None prefix matches any key in the bucket.
        let any =
            trigger(json!({ "kind": "storage_object", "event": "created", "bucket": "docs" }));
        assert!(any.matches(&storage_ev("created", "docs", "anywhere")));
        // The three canonical change kinds each fire their own event.
        let updated =
            trigger(json!({ "kind": "storage_object", "event": "updated", "bucket": "docs" }));
        assert!(updated.matches(&storage_ev("updated", "docs", "k")));
        assert!(!updated.matches(&storage_ev("created", "docs", "k")));
    }

    #[test]
    fn storage_object_extension_filter() {
        // An `extensions` allow-list narrows to matching file types; the check is
        // case-insensitive and each entry's leading dot is optional.
        let office = trigger(json!({
            "kind": "storage_object", "event": "created", "bucket": "docs",
            "extensions": ["docx", ".xlsx", "PPTX"]
        }));
        assert!(office.matches(&storage_ev("created", "docs", "inbox/report.docx")));
        assert!(office.matches(&storage_ev("created", "docs", "sheet.XLSX"))); // key case-insensitive
        assert!(office.matches(&storage_ev("created", "docs", "deck.pptx")));
        // Wrong extension, or no extension, never matches a non-empty list.
        assert!(!office.matches(&storage_ev("created", "docs", "notes.pdf")));
        assert!(!office.matches(&storage_ev("created", "docs", "README")));
        // A dot in a parent segment is not an extension.
        assert!(!office.matches(&storage_ev("created", "docs", "my.dir/file")));
        // The prefix and extension conjuncts AND together.
        let scoped = trigger(json!({
            "kind": "storage_object", "event": "created", "bucket": "docs",
            "prefix": "inbox/", "extensions": ["docx"]
        }));
        assert!(scoped.matches(&storage_ev("created", "docs", "inbox/a.docx")));
        assert!(!scoped.matches(&storage_ev("created", "docs", "archive/a.docx"))); // wrong prefix
        assert!(!scoped.matches(&storage_ev("created", "docs", "inbox/a.pdf"))); // wrong ext

        // The helper directly: empty list ⇒ no constraint; multi-dot keeps last seg.
        assert!(key_has_extension("anything", &[]));
        assert!(key_has_extension("archive.tar.gz", &["gz".into()]));
        assert!(!key_has_extension("archive.tar.gz", &["tar".into()]));
        assert!(!key_has_extension("noext", &["docx".into()]));
    }

    #[test]
    fn collect_triggers_never_match_an_event_and_carry_kind() {
        // Collect triggers are poll-driven (the collect scanner fires them on a
        // cadence) — like Schedule, they never match an ad-hoc TriggerEvent.
        let ce = trigger(json!({ "kind": "collect_email", "connection": "conn-1" }));
        assert!(!ce.matches(&TriggerEvent::Webhook { path: "/x".into() }));
        assert_eq!(ce.kind(), "collect_email");
        assert!(ce.is_collect());
        assert_eq!(ce.collect_connection(), Some("conn-1"));

        let cc = trigger(json!({
            "kind": "collect_calendar", "connection": "conn-2", "commit_on": "write"
        }));
        assert!(!cc.matches(&storage_ev("created", "b", "k")));
        assert_eq!(cc.kind(), "collect_calendar");
        assert_eq!(cc.commit_on(), Some("write"));

        let cs = trigger(json!({
            "kind": "collect_sql", "connection": "conn-3", "tables": "orders_*",
            "cursor_column": "id", "commit_on": "handle"
        }));
        assert!(!cs.matches(&TriggerEvent::Webhook { path: "/x".into() }));
        assert_eq!(cs.kind(), "collect_sql");
        assert!(cs.is_collect());
        assert_eq!(cs.collect_connection(), Some("conn-3"));
        assert_eq!(cs.commit_on(), Some("handle"));
    }

    #[test]
    fn schedule_trigger_never_matches_an_event() {
        let t = trigger(json!({ "kind": "schedule", "cron": "0 9 * * *" }));
        assert!(!t.matches(&TriggerEvent::TaskMoved {
            board: "b".into(),
            to_column: "c".into()
        }));
    }

    fn automation(name: &str, enabled: bool, triggers: Vec<Value>) -> Automation {
        Automation {
            id: catalerum_core::AutomationId::new(),
            workspace_id: catalerum_core::WorkspaceId::new(),
            name: name.into(),
            enabled,
            triggers,
            condition: None,
            actions: vec![json!({ "kind": "summarize" })],
            spec: None,
            grant_id: None,
        }
    }

    #[test]
    fn matching_automations_filters_enabled_matching_and_well_formed() {
        let event = TriggerEvent::TaskMoved {
            board: "sprint".into(),
            to_column: "done".into(),
        };
        let hit = automation(
            "hit",
            true,
            vec![json!({ "kind": "task_moved", "board": "sprint", "to_column": "done" })],
        );
        let disabled = automation(
            "disabled",
            false,
            vec![json!({ "kind": "task_moved", "board": "sprint", "to_column": "done" })],
        );
        let wrong = automation(
            "wrong",
            true,
            vec![json!({ "kind": "task_moved", "board": "sprint", "to_column": "doing" })],
        );
        let malformed = automation("malformed", true, vec![json!({ "kind": "task_moved" })]); // missing fields
                                                                                              // Multiple triggers — any match counts.
        let multi = automation(
            "multi",
            true,
            vec![
                json!({ "kind": "webhook", "path": "/x" }),
                json!({ "kind": "task_moved", "board": "sprint", "to_column": "done" }),
            ],
        );

        let all = vec![hit, disabled, wrong, malformed, multi];
        let matched = matching_automations(&all, &event);
        let names: Vec<&str> = matched.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["hit", "multi"],
            "only enabled + matching + parseable fire"
        );
    }
}
