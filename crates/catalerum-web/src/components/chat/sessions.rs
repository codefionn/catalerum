use super::*;

/// Derive a conversation title from its opening message: the first non-empty
/// line, trimmed to 50 chars. Falls back to "New chat" when empty.
pub(super) fn derive_title(content: &str) -> String {
    let first = content.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let title: String = first.trim().chars().take(50).collect();
    if title.is_empty() {
        "New chat".to_string()
    } else {
        title
    }
}

/// Current wall-clock time as Unix epoch milliseconds (the browser clock). Only
/// reached from the live component, never from native unit tests.
pub(super) fn now_ms() -> i64 {
    js_sys::Date::now() as i64
}

/// The browser's local-time offset from UTC in milliseconds — the value to *add*
/// to a UTC epoch timestamp to get local wall-clock time (`getTimezoneOffset`
/// counts the other way). Only reached from the live component, never from
/// native unit tests.
pub(super) fn local_tz_offset_ms() -> i64 {
    -(js_sys::Date::new_0().get_timezone_offset() as i64) * 60_000
}

/// A collision-resistant object key for a chat upload: `chat/<ms>-<rand>-<name>`
/// (SOUL §9). Mirrors the calendar attachment key scheme — the millisecond stamp
/// orders uploads and the random component keeps same-selection keys distinct; the
/// storage route percent-encodes each segment, so a crafted filename can't escape.
pub(super) fn chat_upload_key(name: &str) -> String {
    let rand = (js_sys::Math::random() * 1_000_000_000.0) as u64;
    format!("chat/{}-{}-{}", now_ms(), rand, name.trim_matches('/'))
}

/// Full month names, indexed by `month - 1`.
const MONTH_FULL: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// Abbreviated month names, indexed by `month - 1`.
const MONTH_ABBR: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Filter conversations by a case-insensitive substring of their title. An empty
/// (or whitespace-only) `query` returns all; an untitled conversation matches only
/// the empty query. Order is preserved, so a later [`group_sessions`] still sees
/// newest-first input.
pub(super) fn filter_conversations(convs: &[Conversation], query: &str) -> Vec<Conversation> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return convs.to_vec();
    }
    convs
        .iter()
        .filter(|c| {
            c.title
                .as_deref()
                .is_some_and(|t| t.to_lowercase().contains(&q))
        })
        .cloned()
        .collect()
}

/// Group conversations into labelled recency runs: "Today", "Yesterday",
/// "Previous 7 days", then one group per week (1–8 weeks old), then one per
/// month. Input is assumed newest-first (the API's `created_at DESC` ordering),
/// so each [`Bucket`] forms a single contiguous run and groups come out in the
/// right order. `tz_offset_ms` shifts UTC timestamps to the user's local
/// wall-clock so day boundaries match their calendar (see [`local_tz_offset_ms`]).
pub(super) fn group_sessions(convs: &[Conversation], now_ms: i64, tz_offset_ms: i64) -> Vec<SessionGroup> {
    let mut groups: Vec<SessionGroup> = Vec::new();
    let mut current: Option<Bucket> = None;
    for c in convs {
        let bucket = match parse_unix_ms(&c.created_at) {
            Some(ms) => classify(ms, now_ms, tz_offset_ms),
            None => Bucket::Unknown,
        };
        if current.as_ref() != Some(&bucket) {
            groups.push(SessionGroup {
                label: bucket_label(&bucket),
                items: Vec::new(),
            });
            current = Some(bucket);
        }
        groups
            .last_mut()
            .expect("a group was just pushed")
            .items
            .push(c.clone());
    }
    groups
}

/// A change-sensitive key for a [`SessionGroup`] in the sidebar's keyed `<For>`.
///
/// A keyed `<For>` never re-renders a child whose key is unchanged, and each
/// group child captures its `items` once (the inner `<For>` reads a plain `Vec`,
/// not a signal). Keying only by the (stable) recency label — "Last 7 days" etc.
/// — therefore froze a group's session list: a conversation created, renamed, or
/// deleted within an existing bucket never re-rendered (the "no chat is added to
/// the sidebar" bug). Folding each conversation's id + title into the key makes
/// the key change whenever the group's contents change, so the group re-renders
/// with a fresh list. Active-row highlighting stays reactive per row (its class
/// reads `active_id`), so it needn't be part of the key.
pub(super) fn group_key(g: &SessionGroup) -> String {
    let mut key = g.label.clone();
    for c in &g.items {
        key.push('\u{1f}'); // unit separator — can't appear in id/title
        key.push_str(&c.id);
        key.push('\u{1e}'); // record separator
        key.push_str(c.title.as_deref().unwrap_or(""));
    }
    key
}

/// Classify a conversation's start time into a recency [`Bucket`]. All
/// boundaries are local calendar days (not rolling 24h windows): `tz_offset_ms`
/// is added to the UTC timestamps before computing the day number.
pub(super) fn classify(created_ms: i64, now_ms: i64, tz_offset_ms: i64) -> Bucket {
    const DAY: i64 = 86_400_000;
    let z = (created_ms + tz_offset_ms).div_euclid(DAY);
    let today = (now_ms + tz_offset_ms).div_euclid(DAY);
    match today - z {
        // A future-dated item (clock skew) still reads as "Today".
        i64::MIN..=0 => Bucket::Today,
        1 => Bucket::Yesterday,
        2..=6 => Bucket::Last7,
        7..=55 => {
            // The Monday of the item's week (Unix day 0 = Thursday, so
            // Monday-based weekday index is `(z + 3) mod 7`).
            let monday = z - (z + 3).rem_euclid(7);
            Bucket::Week(monday)
        }
        _ => {
            let (y, m, _) = civil_from_days(z);
            Bucket::Month(y, m)
        }
    }
}

/// The heading shown for a [`Bucket`].
fn bucket_label(bucket: &Bucket) -> String {
    match bucket {
        Bucket::Today => "Today".to_string(),
        Bucket::Yesterday => "Yesterday".to_string(),
        Bucket::Last7 => "Previous 7 days".to_string(),
        Bucket::Week(monday) => {
            let (_, m, d) = civil_from_days(*monday);
            format!("Week of {} {}", MONTH_ABBR[(m - 1) as usize], d)
        }
        Bucket::Month(y, m) => format!("{} {}", MONTH_FULL[(*m - 1) as usize], y),
        Bucket::Unknown => "Earlier".to_string(),
    }
}

/// Parse an RFC 3339 / ISO-8601 UTC timestamp (`YYYY-MM-DDТHH:MM:SS…`, as the API
/// emits for `created_at`) into Unix epoch milliseconds. Lenient: it reads the
/// date and `HH:MM:SS`, ignoring any fractional seconds and treating the value as
/// UTC (the API always serializes `Z`). Returns `None` on a shape it can't read.
pub(super) fn parse_unix_ms(s: &str) -> Option<i64> {
    let sep = s.find(['T', 't', ' '])?;
    let (date, rest) = (&s[..sep], &s[sep + 1..]);

    let mut dp = date.split('-');
    let y: i64 = dp.next()?.trim().parse().ok()?;
    let mo: i64 = dp.next()?.parse().ok()?;
    let d: i64 = dp.next()?.parse().ok()?;
    if !(1..=12).contains(&mo) {
        return None;
    }

    let hh: i64 = rest.get(0..2)?.parse().ok()?;
    let mm: i64 = rest.get(3..5).and_then(|x| x.parse().ok()).unwrap_or(0);
    let ss: i64 = rest.get(6..8).and_then(|x| x.parse().ok()).unwrap_or(0);

    let days = days_from_civil(y, mo, d);
    Some(days * 86_400_000 + hh * 3_600_000 + mm * 60_000 + ss * 1000)
}

/// Days since the Unix epoch (1970-01-01) for a proleptic-Gregorian civil date.
/// Howard Hinnant's `days_from_civil`.
pub(super) fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// The civil `(year, month, day)` for a count of days since the Unix epoch.
/// Howard Hinnant's `civil_from_days` (inverse of [`days_from_civil`]).
pub(super) fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}
