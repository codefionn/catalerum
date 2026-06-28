//! A focused parser for WebDAV/CalDAV `multistatus` responses (RFC 4918 §13,
//! RFC 4791, RFC 6578), built on `quick-xml`.
//!
//! CalDAV's `REPORT` (both `calendar-query` and `sync-collection`) returns a
//! `DAV:multistatus` of `DAV:response`s. Each response carries a resource `href`
//! and a `propstat` with the resource `getetag` and the CalDAV
//! `calendar-data` (an embedded `VCALENDAR`). A `sync-collection` response also
//! carries a trailing `DAV:sync-token`, and individual responses may report a
//! `404`/`status` to signal a deletion.
//!
//! We match on **local element names** (ignoring namespace prefixes), since
//! servers vary the prefixes they bind to `DAV:` and the CalDAV namespace.
//! Only the elements catalerum needs are extracted; everything else is skipped.

use quick_xml::events::Event as XmlEvent;
use quick_xml::Reader;

use catalerum_core::error::{Error, Result};

/// One `DAV:response` from a `multistatus`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResponseEntry {
    /// The resource path (`DAV:href`), e.g. `/cal/work/evt-1.ics`.
    pub href: String,
    /// `DAV:getetag` for the resource, if present (the per-resource sync token).
    pub etag: Option<String>,
    /// The embedded `urn:ietf:params:xml:ns:caldav:calendar-data` body (a
    /// `VCALENDAR`), if present.
    pub calendar_data: Option<String>,
    /// The HTTP-ish `DAV:status` line for this response, if any. A `404`
    /// indicates the resource was deleted (used by `sync-collection`).
    pub status: Option<String>,
}

impl ResponseEntry {
    /// True when this response reports the resource as gone (HTTP 404 in its
    /// `status`), i.e. a deletion in a `sync-collection` report.
    #[must_use]
    pub fn is_deleted(&self) -> bool {
        self.status
            .as_deref()
            .is_some_and(|s| s.contains("404") && self.calendar_data.is_none())
    }
}

/// A parsed `multistatus`: per-resource responses plus an optional
/// `DAV:sync-token` (present on `sync-collection` reports, RFC 6578).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MultiStatus {
    pub responses: Vec<ResponseEntry>,
    pub sync_token: Option<String>,
}

/// Parse a `multistatus` XML document.
pub fn parse_multistatus(xml: &str) -> Result<MultiStatus> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut result = MultiStatus::default();
    // The element path we currently sit inside, by local name, lowercased.
    let mut stack: Vec<String> = Vec::new();
    let mut current: Option<ResponseEntry> = None;

    loop {
        match reader.read_event() {
            Err(e) => {
                return Err(Error::Provider(format!(
                    "malformed multistatus at {}: {e}",
                    reader.error_position()
                )))
            }
            Ok(XmlEvent::Eof) => break,
            Ok(XmlEvent::Start(start)) => {
                let local = local_name(start.local_name().as_ref());
                if local == "response" {
                    current = Some(ResponseEntry::default());
                }
                stack.push(local);
            }
            Ok(XmlEvent::End(end)) => {
                let local = local_name(end.local_name().as_ref());
                if local == "response" {
                    if let Some(entry) = current.take() {
                        result.responses.push(entry);
                    }
                }
                // Pop the matching frame (best-effort; XML is assumed well-formed).
                if stack.last().map(String::as_str) == Some(local.as_str()) {
                    stack.pop();
                }
            }
            Ok(XmlEvent::Text(text)) => {
                // Decode the (possibly non-UTF-8) bytes, then resolve XML
                // entities (`&amp;`, `&lt;`, …) to their literal characters.
                let decoded = text
                    .decode()
                    .map_err(|e| Error::Provider(format!("xml text decode: {e}")))?;
                let value = quick_xml::escape::unescape(&decoded)
                    .map(|c| c.into_owned())
                    .unwrap_or_else(|_| decoded.into_owned());
                handle_value(value, &stack, &mut current, &mut result);
            }
            Ok(XmlEvent::CData(cdata)) => {
                // CDATA is literal — no entity unescaping. CalDAV servers may
                // wrap `calendar-data` in CDATA when the ICS contains `<`/`&`.
                let value = String::from_utf8_lossy(cdata.as_ref()).into_owned();
                handle_value(value, &stack, &mut current, &mut result);
            }
            _ => {}
        }
    }

    Ok(result)
}

/// Route a decoded text/CDATA value into the right field based on the current
/// element stack.
fn handle_value(
    value: String,
    stack: &[String],
    current: &mut Option<ResponseEntry>,
    result: &mut MultiStatus,
) {
    if value.is_empty() {
        return;
    }
    let leaf = stack.last().map(String::as_str).unwrap_or("");
    match leaf {
        // A document-level (trailing) sync-token belongs to the multistatus.
        "sync-token" if current.is_none() => {
            result.sync_token = Some(value);
        }
        "sync-token" => {
            // Some servers nest sync-token oddly; still capture it.
            result.sync_token.get_or_insert(value);
        }
        "href" => {
            if let Some(entry) = current.as_mut() {
                if entry.href.is_empty() {
                    entry.href = value;
                }
            }
        }
        "getetag" => {
            if let Some(entry) = current.as_mut() {
                entry.etag = Some(strip_quotes(&value));
            }
        }
        "calendar-data" => {
            if let Some(entry) = current.as_mut() {
                // Accumulate, don't overwrite: a server may split the ICS body across
                // multiple text/CDATA events (e.g. text interleaved with a CDATA
                // section), and each arrives as a separate parser event.
                entry
                    .calendar_data
                    .get_or_insert_with(String::new)
                    .push_str(&value);
            }
        }
        "status" => {
            // Only a *response-level* `<status>` is the deletion signal (RFC 6578).
            // A `<status>` inside a `<propstat>` reports the fate of one property
            // group, not the resource — capturing it would let a `404` propstat (a
            // requested-but-absent property on an *existing* resource, which RFC 4918
            // §13 returns in its own propstat) masquerade as a deletion and wrongly
            // drop the event on sync.
            let parent = stack
                .len()
                .checked_sub(2)
                .and_then(|i| stack.get(i))
                .map(String::as_str);
            if parent == Some("response") {
                if let Some(entry) = current.as_mut() {
                    entry.status = Some(value);
                }
            }
        }
        _ => {}
    }
}

/// Lowercase the local part of a (possibly prefixed) element name.
fn local_name(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).to_ascii_lowercase()
}

/// ETags arrive quoted (`"abc"`, or weak `W/"abc"`); store the bare value.
fn strip_quotes(s: &str) -> String {
    let s = s.trim();
    let s = s.strip_prefix("W/").unwrap_or(s);
    s.trim_matches('"').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // A recorded CalDAV sync-collection multistatus sample (RFC 6578 shape):
    // one updated resource (with calendar-data + etag) and one deleted (404).
    const SYNC_SAMPLE: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:response>
    <D:href>/cal/work/evt-1.ics</D:href>
    <D:propstat>
      <D:prop>
        <D:getetag>"etag-aaa"</D:getetag>
        <C:calendar-data>BEGIN:VCALENDAR
VERSION:2.0
BEGIN:VEVENT
UID:evt-1@dav
DTSTART:20260613T090000Z
DTEND:20260613T100000Z
SUMMARY:Synced meeting
END:VEVENT
END:VCALENDAR
</C:calendar-data>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
  <D:response>
    <D:href>/cal/work/evt-deleted.ics</D:href>
    <D:status>HTTP/1.1 404 Not Found</D:status>
  </D:response>
  <D:sync-token>http://example.com/sync/42</D:sync-token>
</D:multistatus>"#;

    #[test]
    fn parses_sync_collection_with_token_and_deletion() {
        let ms = parse_multistatus(SYNC_SAMPLE).expect("parse");
        assert_eq!(ms.sync_token.as_deref(), Some("http://example.com/sync/42"));
        assert_eq!(ms.responses.len(), 2);

        let updated = &ms.responses[0];
        assert_eq!(updated.href, "/cal/work/evt-1.ics");
        assert_eq!(updated.etag.as_deref(), Some("etag-aaa"));
        assert!(updated
            .calendar_data
            .as_deref()
            .unwrap()
            .contains("UID:evt-1@dav"));
        assert!(!updated.is_deleted());

        let deleted = &ms.responses[1];
        assert_eq!(deleted.href, "/cal/work/evt-deleted.ics");
        assert!(deleted.is_deleted());
        assert!(deleted.calendar_data.is_none());
    }

    #[test]
    fn split_propstat_404_does_not_look_like_a_deletion() {
        // RFC 4918 §13 lets a server split a response into a `200` propstat for the
        // found props and a `404` propstat for requested-but-absent ones (here
        // `calendar-data`). The resource still EXISTS — only a *response-level*
        // status signals deletion — so this must not be treated as a deletion (which
        // would wrongly drop the event on sync). Before the parent-element check the
        // `404` propstat status leaked into `entry.status` and, with no
        // `calendar-data` present, `is_deleted()` returned true.
        let xml = r#"<multistatus xmlns="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <response>
    <href>/c/exists.ics</href>
    <propstat>
      <prop><getetag>"v9"</getetag></prop>
      <status>HTTP/1.1 200 OK</status>
    </propstat>
    <propstat>
      <prop><c:calendar-data/></prop>
      <status>HTTP/1.1 404 Not Found</status>
    </propstat>
  </response>
</multistatus>"#;
        let ms = parse_multistatus(xml).expect("parse");
        let r = &ms.responses[0];
        assert_eq!(r.etag.as_deref(), Some("v9"));
        assert!(r.calendar_data.is_none());
        // No response-level status was present, so the entry is not flagged deleted.
        assert_eq!(r.status, None, "propstat statuses must not be captured");
        assert!(
            !r.is_deleted(),
            "a split-propstat 404 must not look like a deletion"
        );
    }

    #[test]
    fn response_level_status_is_still_captured() {
        // A genuine sync-collection deletion keeps a response-level `<status>` —
        // that one must still be captured so `is_deleted()` works.
        let xml = r#"<multistatus xmlns="DAV:">
  <response>
    <href>/c/gone.ics</href>
    <status>HTTP/1.1 404 Not Found</status>
  </response>
</multistatus>"#;
        let ms = parse_multistatus(xml).expect("parse");
        assert!(ms.responses[0].is_deleted());
    }

    #[test]
    fn calendar_data_spanning_text_and_cdata_is_accumulated() {
        // A server interleaves plain text with a CDATA section inside calendar-data;
        // every fragment must survive (the old overwrite kept only the last event).
        let xml = r#"<multistatus xmlns="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <response>
    <href>/c/a.ics</href>
    <propstat>
      <prop>
        <getetag>"v1"</getetag>
        <c:calendar-data>BEGIN:VCALENDAR<![CDATA[ X&Y ]]>END:VCALENDAR</c:calendar-data>
      </prop>
      <status>HTTP/1.1 200 OK</status>
    </propstat>
  </response>
</multistatus>"#;
        let ms = parse_multistatus(xml).expect("parse");
        let data = ms.responses[0].calendar_data.as_deref().unwrap();
        assert!(
            data.contains("BEGIN:VCALENDAR"),
            "leading text kept: {data}"
        );
        assert!(data.contains("X&Y"), "CDATA fragment kept: {data}");
        assert!(data.contains("END:VCALENDAR"), "trailing text kept: {data}");
    }

    #[test]
    fn strips_weak_and_strong_etag_quotes() {
        assert_eq!(strip_quotes("\"abc\""), "abc");
        assert_eq!(strip_quotes("W/\"abc\""), "abc");
        assert_eq!(strip_quotes("bare"), "bare");
    }

    #[test]
    fn calendar_query_without_sync_token() {
        let xml = r#"<multistatus xmlns="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <response>
    <href>/c/a.ics</href>
    <propstat>
      <prop>
        <getetag>"v1"</getetag>
        <c:calendar-data>BEGIN:VCALENDAR
BEGIN:VEVENT
UID:a
DTSTART:20260101T000000Z
SUMMARY:A
END:VEVENT
END:VCALENDAR</c:calendar-data>
      </prop>
      <status>HTTP/1.1 200 OK</status>
    </propstat>
  </response>
</multistatus>"#;
        let ms = parse_multistatus(xml).expect("parse");
        assert!(ms.sync_token.is_none());
        assert_eq!(ms.responses.len(), 1);
        assert_eq!(ms.responses[0].etag.as_deref(), Some("v1"));
    }
}
