//! Shared iCalendar (RFC 5545) parsing, used by every backend that speaks
//! `.ics`: the local-directory provider and CalDAV/webcal (whose `REPORT` and
//! `GET` responses embed `VEVENT` bodies).
//!
//! The job here is to turn a `VCALENDAR` document — or a single embedded
//! `VEVENT` body — into core [`Event`](catalerum_core::Event)s, mapping the
//! fields catalerum cares about (SOUL §8, §15): `UID`, `DTSTART`/`DTEND`
//! (including all-day `VALUE=DATE`), `RRULE`, `SUMMARY`, `LOCATION`,
//! `DESCRIPTION`, `ATTENDEE`, and `SEQUENCE`, faithfully.
//!
//! ## Attendees (M2 scope)
//! Core [`Event::attendees`](catalerum_core::Event) is `Vec<EntityRef>` — a
//! pointer at a *catalogued* [`Entity`](catalerum_core::Entity). At sync time no
//! entities are catalogued yet (entity extraction / dedup is the ingestion +
//! graph phase, SOUL §10 / M4), so we cannot synthesise an `EntityId` here.
//! Parsed `ATTENDEE` addresses are therefore exposed verbatim on
//! [`ParsedEvent::attendees`] for the ingest pipeline to resolve, while the
//! produced [`Event::attendees`] stays empty. This keeps the provider layer
//! provider-faithful without inventing identity.

use chrono::{DateTime, NaiveDate, NaiveTime, TimeZone, Utc};
use icalendar::parser as ical_parser;
use icalendar::{
    Calendar as IcalCalendar, CalendarComponent, Component, DatePerhapsTime, EventLike,
};

use catalerum_core::error::{Error, Result};
use catalerum_core::id::{CalendarId, EventId, WorkspaceId};
use catalerum_core::model::{Attachment, Event};

/// A single `VEVENT` parsed from iCalendar text, in the provider's own terms
/// (before it is catalogued into a core [`Event`]).
///
/// The raw [`attendees`](Self::attendees) carry information the core [`Event`]
/// cannot (yet) hold; the rest — including [`all_day`](Self::all_day) — map 1:1.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedEvent {
    /// iCalendar `UID` (stable across edits). Synthesised if the source omits it.
    pub uid: String,
    /// `DTSTART`, resolved to UTC (midnight UTC for all-day dates).
    pub start: DateTime<Utc>,
    /// `DTEND` (or `DTSTART` + `DURATION`, else `DTSTART`), resolved to UTC.
    pub end: DateTime<Utc>,
    /// True when `DTSTART`/`DTEND` are `VALUE=DATE` (a whole-day event).
    pub all_day: bool,
    /// `RRULE`, verbatim (without the `RRULE:` prefix).
    pub rrule: Option<String>,
    pub summary: String,
    pub location: Option<String>,
    /// `DESCRIPTION`.
    pub body: Option<String>,
    /// `ATTENDEE` calendar addresses (e.g. `mailto:a@b.com`), verbatim. Resolved
    /// into entities later by the ingest pipeline (see module docs).
    pub attendees: Vec<String>,
    /// `CATEGORIES`, flattened across every `CATEGORIES` line and split on commas
    /// (trimmed, blanks dropped). Become the event's labels.
    pub labels: Vec<String>,
    /// `ATTACH` properties — one [`Attachment`] each, carrying the URI plus the
    /// `FMTTYPE` (content type) and `FILENAME` params when present.
    pub attachments: Vec<Attachment>,
    /// `SEQUENCE` (defaults to 0 when absent).
    pub sequence: i64,
}

impl ParsedEvent {
    /// Promote into a core [`Event`] under `workspace_id` / `calendar_id`.
    ///
    /// A fresh random [`EventId`] is assigned; the store upserts by
    /// `(calendar_id, uid)`, so identity stays the iCalendar `UID` and the id
    /// is irrelevant to idempotency. `attendees` is left empty by design (see
    /// module docs).
    #[must_use]
    pub fn into_event(self, workspace_id: WorkspaceId, calendar_id: CalendarId) -> Event {
        Event {
            id: EventId::new(),
            workspace_id,
            calendar_id,
            uid: self.uid,
            start: self.start,
            end: self.end,
            all_day: self.all_day,
            rrule: self.rrule,
            summary: self.summary,
            location: self.location,
            attendees: Vec::new(),
            body: self.body,
            labels: self.labels,
            attachments: self.attachments,
            etag: None,
            sequence: self.sequence,
        }
    }
}

/// Parse a full `VCALENDAR` document into its `VEVENT`s.
///
/// Folded lines (RFC 5545 §3.1) are handled by `icalendar`. Components that are
/// not `VEVENT` (`VTODO`, `VTIMEZONE`, …) are ignored. Returns an
/// [`Error::Provider`] only on a hard parse failure; a calendar with zero
/// events yields an empty vector.
pub fn parse_calendar(ics: &str) -> Result<Vec<ParsedEvent>> {
    // `icalendar` exposes two parse paths; the high-level `Calendar::from_str`
    // (via the parser feature) gives typed `Event` accessors. We avoid the
    // `FromStr` trait import noise by going through the parser module directly,
    // which also yields readable, line-numbered errors.
    let unfolded = ical_parser::unfold(ics);
    let parsed = ical_parser::read_calendar(&unfolded)
        .map_err(|e| Error::Provider(format!("invalid iCalendar: {e}")))?;
    let calendar: IcalCalendar = parsed.into();

    let mut out = Vec::new();
    for component in &calendar.components {
        if let CalendarComponent::Event(event) = component {
            out.push(parse_event(event)?);
        }
    }
    Ok(out)
}

/// Parse the events from an arbitrary iCalendar fragment that may or may not be
/// wrapped in `BEGIN:VCALENDAR`. CalDAV `REPORT` responses embed a complete
/// `VCALENDAR` per resource, so this just delegates to [`parse_calendar`]; the
/// helper exists so callers read intent at the call site.
pub fn parse_vevents(ics: &str) -> Result<Vec<ParsedEvent>> {
    parse_calendar(ics)
}

/// Serialize a core [`Event`] into a standalone `VCALENDAR` document holding one
/// `VEVENT` — the write half of this module (CalDAV `PUT` bodies, SOUL §8
/// write-back). The mapped fields mirror the parse half exactly: `UID`,
/// `DTSTART`/`DTEND` (all-day events become `VALUE=DATE` dates), `RRULE`,
/// `SUMMARY`, `LOCATION`, `DESCRIPTION`, `CATEGORIES` (one line per label, so a
/// label's own commas survive), `ATTACH` (with `FMTTYPE`/`FILENAME` params), and
/// `SEQUENCE`. Text escaping/folding is the `icalendar` crate's.
///
/// `attendees` ride separately in provider-native form (`mailto:` is prefixed
/// onto a bare address): the core event's `attendees` are resolved *entity
/// pointers* that carry no calendar address, so only a caller that still holds
/// the raw addresses (event creation) can write them.
pub fn event_to_ics(event: &Event, attendees: &[String]) -> String {
    let mut ev = icalendar::Event::new();
    ev.uid(&event.uid)
        .summary(&event.summary)
        .timestamp(Utc::now());
    if event.all_day {
        // All-day stamps mark calendar dates (midnight UTC by convention).
        ev.starts(event.start.date_naive());
        ev.ends(event.end.date_naive());
    } else {
        ev.starts(event.start);
        ev.ends(event.end);
    }
    if let Some(location) = event.location.as_deref().filter(|s| !s.is_empty()) {
        ev.location(location);
    }
    if let Some(body) = event.body.as_deref().filter(|s| !s.is_empty()) {
        ev.description(body);
    }
    if let Some(rrule) = event
        .rrule
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        ev.add_property("RRULE", rrule);
    }
    for label in &event.labels {
        let label = label.trim();
        if !label.is_empty() {
            ev.add_multi_property("CATEGORIES", label);
        }
    }
    for attendee in attendees {
        let a = attendee.trim();
        if a.is_empty() {
            continue;
        }
        // A bare email becomes a proper calendar address; an explicit scheme
        // (`mailto:` already present) is kept verbatim.
        let addr = if a.contains(':') {
            a.to_string()
        } else {
            format!("mailto:{a}")
        };
        ev.add_multi_property("ATTENDEE", &addr);
    }
    for att in &event.attachments {
        if att.url.trim().is_empty() {
            continue;
        }
        let mut prop = icalendar::Property::new("ATTACH", att.url.trim());
        if let Some(ct) = att.content_type.as_deref().filter(|s| !s.is_empty()) {
            prop.add_parameter("FMTTYPE", ct);
        }
        if let Some(name) = att.filename.as_deref().filter(|s| !s.is_empty()) {
            prop.add_parameter("FILENAME", name);
        }
        ev.append_multi_property(prop.done());
    }
    if event.sequence > 0 {
        ev.sequence(u32::try_from(event.sequence).unwrap_or(u32::MAX));
    }
    let mut cal = IcalCalendar::new();
    cal.push(ev.done());
    cal.to_string()
}

fn parse_event(event: &icalendar::Event) -> Result<ParsedEvent> {
    let uid = event
        .get_uid()
        .filter(|u| !u.is_empty())
        .map_or_else(synthesize_uid, |u| u.to_string());

    let start_dpt = event
        .get_start()
        .ok_or_else(|| Error::Provider(format!("event {uid} has no DTSTART")))?;
    let (start, start_all_day) = resolve(&start_dpt)?;

    // DTEND is optional; for all-day events RFC 5545 makes DTEND exclusive.
    // When absent, an event may instead carry a DURATION (RFC 5545 §3.6.1) — the
    // `icalendar` crate's `get_end` only reads DTEND, so resolve DURATION here.
    // With neither, an all-day event is one day long and a timed event is
    // zero-length (start == end) — matching common client behaviour.
    let (end, end_all_day) = match event.get_end() {
        Some(end_dpt) => resolve(&end_dpt)?,
        None => match event
            .property_value("DURATION")
            .and_then(parse_ical_duration)
        {
            Some(dur) => (start + dur, start_all_day),
            None if start_all_day => (start + chrono::Duration::days(1), true),
            None => (start, false),
        },
    };

    let rrule = event
        .property_value("RRULE")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let attendees = event
        .get_attendees()
        .into_iter()
        .map(|a| a.cal_address)
        .filter(|a| !a.is_empty())
        .collect();

    Ok(ParsedEvent {
        uid,
        start,
        end,
        all_day: start_all_day || end_all_day,
        rrule,
        summary: event.get_summary().unwrap_or_default().to_string(),
        location: event
            .get_location()
            .map(str::to_string)
            .filter(|s| !s.is_empty()),
        body: event
            .get_description()
            .map(str::to_string)
            .filter(|s| !s.is_empty()),
        attendees,
        labels: parse_labels(event),
        attachments: parse_attachments(event),
        sequence: i64::from(event.get_sequence().unwrap_or(0)),
    })
}

/// Collect an event's `CATEGORIES` into a flat, de-duplicated label list.
///
/// `CATEGORIES` is a comma-separated list and may appear on several lines
/// (`icalendar` parks every occurrence in `multi_properties`); we flatten across
/// all of them, split each on commas, trim, drop blanks, and dedup
/// case-insensitively (keeping the first-seen casing).
fn parse_labels(event: &icalendar::Event) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    if let Some(props) = event.multi_properties().get("CATEGORIES") {
        for prop in props {
            for raw in prop.value().split(',') {
                let label = raw.trim();
                if !label.is_empty() && seen.insert(label.to_lowercase()) {
                    out.push(label.to_string());
                }
            }
        }
    }
    out
}

/// Collect an event's `ATTACH` properties into [`Attachment`]s.
///
/// Each `ATTACH` (URI form) becomes one attachment carrying the URI plus its
/// `FMTTYPE` (content type) and `FILENAME` / `X-FILENAME` params when present.
/// Empty values are skipped. Inline `VALUE=BINARY` payloads (base64 blobs) are
/// not materialised into object storage here — that is a provider-sync concern;
/// their data URI, if any, is kept verbatim.
fn parse_attachments(event: &icalendar::Event) -> Vec<Attachment> {
    let param = |prop: &icalendar::Property, key: &str| {
        prop.get_param_as(key, |s| {
            let v = s.trim();
            (!v.is_empty()).then(|| v.to_string())
        })
    };
    event
        .multi_properties()
        .get("ATTACH")
        .into_iter()
        .flatten()
        .filter_map(|prop| {
            let url = prop.value().trim().to_string();
            if url.is_empty() {
                return None;
            }
            Some(Attachment {
                url,
                filename: param(prop, "FILENAME").or_else(|| param(prop, "X-FILENAME")),
                content_type: param(prop, "FMTTYPE"),
                size: prop.get_param_as("SIZE", |s| s.trim().parse::<u64>().ok()),
            })
        })
        .collect()
}

/// Resolve a [`DatePerhapsTime`] to an absolute UTC instant plus an `all_day`
/// flag. Floating (timezone-less) date-times are interpreted as UTC — the
/// best provider-agnostic guess when no `TZID` and no `VTIMEZONE` are present.
fn resolve(dpt: &DatePerhapsTime) -> Result<(DateTime<Utc>, bool)> {
    match dpt {
        DatePerhapsTime::Date(date) => Ok((date_to_utc(*date), true)),
        DatePerhapsTime::DateTime(cdt) => {
            // `try_into_utc` handles `Utc` and `WithTimezone` (via chrono-tz);
            // it returns `None` for floating times, which we read as UTC.
            if let Some(utc) = cdt.try_into_utc() {
                return Ok((utc, false));
            }
            match cdt {
                icalendar::CalendarDateTime::Floating(naive) => {
                    Ok((Utc.from_utc_datetime(naive), false))
                }
                // Unknown/unresolvable TZID: fall back to interpreting the wall
                // clock as UTC rather than dropping the event.
                icalendar::CalendarDateTime::WithTimezone { date_time, .. } => {
                    Ok((Utc.from_utc_datetime(date_time), false))
                }
                icalendar::CalendarDateTime::Utc(utc) => Ok((*utc, false)),
            }
        }
    }
}

fn date_to_utc(date: NaiveDate) -> DateTime<Utc> {
    Utc.from_utc_datetime(&date.and_time(NaiveTime::MIN))
}

/// Parse an RFC 5545 `DURATION` value into a [`chrono::Duration`].
///
/// Handles the week form (`P2W`), the date/time form (`P1DT12H`, `PT1H30M`,
/// `PT45S`) and an optional leading sign (`-PT15M`). iCalendar durations have no
/// month/year component (only weeks/days/hours/minutes/seconds), so a stray `M`
/// before the `T` (which would mean months) and any other malformed input return
/// `None`. Overflow is checked rather than panicking.
fn parse_ical_duration(s: &str) -> Option<chrono::Duration> {
    let s = s.trim();
    let (neg, rest) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let rest = rest.strip_prefix('P')?;
    let secs: i64 = if let Some(weeks) = rest.strip_suffix('W') {
        weeks.parse::<i64>().ok()?.checked_mul(7 * 24 * 3600)?
    } else {
        let mut total: i64 = 0;
        let mut num = String::new();
        let mut in_time = false;
        let mut saw_any = false;
        for ch in rest.chars() {
            match ch {
                '0'..='9' => num.push(ch),
                'T' if num.is_empty() => in_time = true,
                'D' | 'H' | 'M' | 'S' => {
                    let n: i64 = num.parse().ok()?;
                    num.clear();
                    let unit = match (ch, in_time) {
                        ('D', false) => 24 * 3600,
                        ('H', true) => 3600,
                        ('M', true) => 60,
                        ('S', true) => 1,
                        _ => return None, // 'M' before 'T' = months (invalid here), etc.
                    };
                    total = total.checked_add(n.checked_mul(unit)?)?;
                    saw_any = true;
                }
                _ => return None,
            }
        }
        // Reject trailing digits with no unit, and an empty `P`/`PT`.
        if !num.is_empty() || !saw_any {
            return None;
        }
        total
    };
    Some(chrono::Duration::seconds(if neg { -secs } else { secs }))
}

/// Last-resort `UID` for a malformed event missing one: stable for identical
/// content within a process is not required (the store upserts by UID), but we
/// keep it unique so distinct events never collide.
fn synthesize_uid() -> String {
    format!("catalerum-nouid-{}", uuid_v4())
}

fn uuid_v4() -> String {
    // catalerum-core already depends on `uuid`; reach it transitively through a
    // core id to avoid a direct dep just for this fallback.
    EventId::new().as_uuid().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
PRODID:-//catalerum//test//EN\r
BEGIN:VEVENT\r
UID:evt-1@catalerum\r
DTSTAMP:20260101T000000Z\r
DTSTART:20260613T090000Z\r
DTEND:20260613T100000Z\r
SUMMARY:Design review\r
LOCATION:Room 4\r
DESCRIPTION:Quarterly design review\r
RRULE:FREQ=WEEKLY;BYDAY=MO\r
SEQUENCE:3\r
CATEGORIES:Work,Design\r
CATEGORIES:work\r
ATTENDEE;CN=Ada:mailto:ada@example.com\r
ATTENDEE:mailto:bob@example.com\r
ATTACH;FMTTYPE=application/pdf;FILENAME=brief.pdf:https://example.com/brief.pdf\r
ATTACH:https://example.com/floorplan.png\r
END:VEVENT\r
BEGIN:VEVENT\r
UID:evt-2@catalerum\r
DTSTART;VALUE=DATE:20260620\r
DTEND;VALUE=DATE:20260621\r
SUMMARY:Company offsite\r
END:VEVENT\r
END:VCALENDAR\r
";

    #[test]
    fn parses_timed_event_with_all_fields() {
        let events = parse_calendar(SAMPLE).expect("parse");
        let e = events.iter().find(|e| e.uid == "evt-1@catalerum").unwrap();
        assert_eq!(e.summary, "Design review");
        assert_eq!(e.location.as_deref(), Some("Room 4"));
        assert_eq!(e.body.as_deref(), Some("Quarterly design review"));
        assert_eq!(e.rrule.as_deref(), Some("FREQ=WEEKLY;BYDAY=MO"));
        assert_eq!(e.sequence, 3);
        assert!(!e.all_day);
        assert_eq!(e.start, Utc.with_ymd_and_hms(2026, 6, 13, 9, 0, 0).unwrap());
        assert_eq!(e.end, Utc.with_ymd_and_hms(2026, 6, 13, 10, 0, 0).unwrap());
        assert_eq!(
            e.attendees,
            vec![
                "mailto:ada@example.com".to_string(),
                "mailto:bob@example.com".to_string()
            ]
        );
        // CATEGORIES flatten across lines, split on commas, dedup
        // case-insensitively (first-seen casing wins: "Work", not "work").
        assert_eq!(e.labels, vec!["Work".to_string(), "Design".to_string()]);
        // Both ATTACH lines parse; the first carries FMTTYPE + FILENAME.
        assert_eq!(e.attachments.len(), 2);
        assert_eq!(e.attachments[0].url, "https://example.com/brief.pdf");
        assert_eq!(
            e.attachments[0].content_type.as_deref(),
            Some("application/pdf")
        );
        assert_eq!(e.attachments[0].filename.as_deref(), Some("brief.pdf"));
        assert_eq!(e.attachments[1].url, "https://example.com/floorplan.png");
        assert!(e.attachments[1].content_type.is_none());
    }

    #[test]
    fn parses_all_day_event() {
        let events = parse_calendar(SAMPLE).expect("parse");
        let e = events.iter().find(|e| e.uid == "evt-2@catalerum").unwrap();
        assert!(e.all_day);
        assert_eq!(e.start, Utc.with_ymd_and_hms(2026, 6, 20, 0, 0, 0).unwrap());
        assert_eq!(e.end, Utc.with_ymd_and_hms(2026, 6, 21, 0, 0, 0).unwrap());
    }

    #[test]
    fn parse_is_idempotent() {
        let a = parse_calendar(SAMPLE).expect("parse a");
        let b = parse_calendar(SAMPLE).expect("parse b");
        // UID/timestamps/content are identical across re-parses; only the
        // randomly-assigned EventId differs (assigned in `into_event`).
        let strip = |v: &[ParsedEvent]| v.to_vec();
        assert_eq!(strip(&a), strip(&b));
    }

    #[test]
    fn timed_event_with_duration_instead_of_dtend() {
        // RFC 5545 lets an event carry DURATION in place of DTEND.
        let ics = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:d1\n\
                   DTSTART:20260613T090000Z\nDURATION:PT1H30M\nSUMMARY:Call\n\
                   END:VEVENT\nEND:VCALENDAR\n";
        let e = &parse_calendar(ics).expect("parse")[0];
        assert!(!e.all_day);
        assert_eq!(e.start, Utc.with_ymd_and_hms(2026, 6, 13, 9, 0, 0).unwrap());
        assert_eq!(e.end, Utc.with_ymd_and_hms(2026, 6, 13, 10, 30, 0).unwrap());
    }

    #[test]
    fn all_day_event_with_multi_day_duration() {
        let ics = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:d2\n\
                   DTSTART;VALUE=DATE:20260620\nDURATION:P3D\nSUMMARY:Trip\n\
                   END:VEVENT\nEND:VCALENDAR\n";
        let e = &parse_calendar(ics).expect("parse")[0];
        assert!(e.all_day);
        assert_eq!(e.end - e.start, chrono::Duration::days(3));
    }

    #[test]
    fn dtend_wins_over_duration_when_both_present() {
        // DTEND is authoritative; DURATION is only the fallback.
        let ics = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:d3\n\
                   DTSTART:20260613T090000Z\nDTEND:20260613T093000Z\nDURATION:PT5H\n\
                   SUMMARY:Short\nEND:VEVENT\nEND:VCALENDAR\n";
        let e = &parse_calendar(ics).expect("parse")[0];
        assert_eq!(e.end - e.start, chrono::Duration::minutes(30));
    }

    #[test]
    fn ical_duration_parsing() {
        use chrono::Duration;
        assert_eq!(parse_ical_duration("PT1H"), Some(Duration::hours(1)));
        assert_eq!(parse_ical_duration("PT30M"), Some(Duration::minutes(30)));
        assert_eq!(parse_ical_duration("PT45S"), Some(Duration::seconds(45)));
        assert_eq!(parse_ical_duration("P1D"), Some(Duration::days(1)));
        assert_eq!(parse_ical_duration("P2W"), Some(Duration::weeks(2)));
        assert_eq!(
            parse_ical_duration("P1DT12H30M"),
            Some(Duration::days(1) + Duration::hours(12) + Duration::minutes(30))
        );
        assert_eq!(parse_ical_duration("-PT15M"), Some(Duration::minutes(-15)));
        assert_eq!(parse_ical_duration("+PT15M"), Some(Duration::minutes(15)));
        // Malformed / unsupported forms reject rather than mis-parse.
        for bad in [
            "", "P", "PT", "1H", "PT1X", "P1M", "PT1H5", "garbage", "PTM",
        ] {
            assert_eq!(parse_ical_duration(bad), None, "{bad:?} should not parse");
        }
    }

    #[test]
    fn single_categories_and_attach_occurrence_are_captured() {
        // `parse_labels`/`parse_attachments` read from `multi_properties()`; the
        // SAMPLE only exercises the *multi*-occurrence path, so guard the common
        // single-occurrence case (a lone CATEGORIES / ATTACH line) explicitly —
        // a future refactor that special-cased "repeated" properties could
        // otherwise silently drop a solo label/attachment without tripping a test.
        let ics = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:s1\n\
                   DTSTART:20260613T090000Z\nSUMMARY:One\n\
                   CATEGORIES:Solo\n\
                   ATTACH:https://example.com/only.pdf\n\
                   END:VEVENT\nEND:VCALENDAR\n";
        let e = &parse_calendar(ics).expect("parse")[0];
        assert_eq!(
            e.labels,
            vec!["Solo".to_string()],
            "single CATEGORIES captured"
        );
        assert_eq!(e.attachments.len(), 1, "single ATTACH captured");
        assert_eq!(e.attachments[0].url, "https://example.com/only.pdf");
    }

    #[test]
    fn all_day_without_dtend_spans_one_day() {
        let ics = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:x\nDTSTART;VALUE=DATE:20260101\nSUMMARY:NYD\nEND:VEVENT\nEND:VCALENDAR\n";
        let events = parse_calendar(ics).expect("parse");
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert!(e.all_day);
        assert_eq!(e.end - e.start, chrono::Duration::days(1));
    }

    // --- serialization (the write half) round-trips through the parse half ---

    fn writable_event() -> Event {
        Event {
            id: EventId::new(),
            workspace_id: WorkspaceId::new(),
            calendar_id: CalendarId::new(),
            uid: "write-1@catalerum".into(),
            start: Utc.with_ymd_and_hms(2026, 7, 10, 9, 30, 0).unwrap(),
            end: Utc.with_ymd_and_hms(2026, 7, 10, 10, 0, 0).unwrap(),
            all_day: false,
            rrule: Some("FREQ=WEEKLY;BYDAY=FR".into()),
            summary: "Standup; the fast one".into(),
            location: Some("Room 1, west wing".into()),
            attendees: Vec::new(),
            body: Some("line one\nline two".into()),
            labels: vec!["Work".into(), "team sync".into()],
            attachments: vec![Attachment {
                url: "https://files.example/brief.pdf".into(),
                filename: Some("brief.pdf".into()),
                content_type: Some("application/pdf".into()),
                size: None,
            }],
            etag: None,
            sequence: 2,
        }
    }

    #[test]
    fn serialized_event_round_trips_through_the_parser() {
        let event = writable_event();
        let ics = event_to_ics(
            &event,
            &["a@example.com".into(), "mailto:b@example.com".into()],
        );
        let parsed = parse_calendar(&ics).unwrap();
        assert_eq!(parsed.len(), 1);
        let p = &parsed[0];
        assert_eq!(p.uid, event.uid);
        assert_eq!(p.start, event.start);
        assert_eq!(p.end, event.end);
        assert!(!p.all_day);
        assert_eq!(p.rrule, event.rrule);
        // Text escaping survives: `;`, `,` and newlines round-trip.
        assert_eq!(p.summary, event.summary);
        assert_eq!(p.location, event.location);
        assert_eq!(p.body, event.body);
        assert_eq!(p.labels, event.labels);
        assert_eq!(p.sequence, event.sequence);
        assert_eq!(p.attachments.len(), 1);
        assert_eq!(p.attachments[0].url, event.attachments[0].url);
        assert_eq!(p.attachments[0].filename, event.attachments[0].filename);
        assert_eq!(
            p.attachments[0].content_type,
            event.attachments[0].content_type
        );
        // Both attendee forms land as proper calendar addresses.
        assert_eq!(
            p.attendees,
            vec![
                "mailto:a@example.com".to_string(),
                "mailto:b@example.com".to_string()
            ]
        );
    }

    #[test]
    fn serialized_all_day_event_uses_date_values() {
        let mut event = writable_event();
        event.all_day = true;
        event.rrule = None;
        event.start = Utc.with_ymd_and_hms(2026, 7, 10, 0, 0, 0).unwrap();
        event.end = Utc.with_ymd_and_hms(2026, 7, 11, 0, 0, 0).unwrap();
        let ics = event_to_ics(&event, &[]);
        assert!(
            ics.contains("DTSTART;VALUE=DATE:20260710"),
            "all-day start must be a DATE value: {ics}"
        );
        let parsed = parse_calendar(&ics).unwrap();
        assert!(parsed[0].all_day);
        assert_eq!(parsed[0].start, event.start);
        assert_eq!(parsed[0].end, event.end);
    }
}
