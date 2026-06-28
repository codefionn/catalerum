//! Cron evaluation for `Schedule { cron }` triggers (SOUL §11).
//!
//! The push-driven triggers (`TaskMoved`, `Webhook`, …) match an ad-hoc event
//! ([`crate::Trigger::matches`]); the **time-driven** `Schedule` trigger instead
//! fires on a clock. A scheduler ticks periodically and, for each enabled
//! `Schedule` automation, asks [`due_in_window`] whether the cron has a scheduled
//! occurrence in the elapsed window — if so it enqueues a `run_automation` job.
//!
//! Crons are **5-field POSIX** (`min hour day-of-month month day-of-week`) and
//! evaluated in the `Schedule` trigger's IANA **timezone** (`tz`, e.g.
//! `"America/New_York"`); absent → **UTC**. So `0 9 * * *` with `tz =
//! "America/New_York"` fires at 9am New York time (DST-aware), not 9am UTC.

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use croner::Cron;

/// A cron string (or timezone) that does not parse or cannot be evaluated.
#[derive(Debug, thiserror::Error)]
#[error("invalid cron `{cron}`: {message}")]
pub struct CronError {
    pub cron: String,
    pub message: String,
}

/// Resolve the IANA timezone the cron is evaluated in: `None` → UTC, else the
/// named zone (e.g. `"Europe/Berlin"`).
fn resolve_tz(cron: &str, tz: Option<&str>) -> Result<Tz, CronError> {
    match tz {
        None => Ok(Tz::UTC),
        Some(name) => name.parse::<Tz>().map_err(|_| CronError {
            cron: cron.to_string(),
            message: format!("unknown timezone `{name}`"),
        }),
    }
}

/// Parse + validate a 5-field POSIX cron string and its optional IANA timezone.
/// Authoring-time check so a `Schedule` trigger with a bad cron or unknown
/// timezone is rejected before it's stored, rather than silently never firing.
///
/// # Errors
/// [`CronError`] if `cron` is not a valid cron expression or `tz` is unknown.
pub fn validate(cron: &str, tz: Option<&str>) -> Result<(), CronError> {
    resolve_tz(cron, tz)?;
    parse(cron).map(|_| ())
}

fn parse(cron: &str) -> Result<Cron, CronError> {
    Cron::new(cron).parse().map_err(|e| CronError {
        cron: cron.to_string(),
        message: e.to_string(),
    })
}

/// Whether `cron` (evaluated in timezone `tz`, or UTC if `None`) has a scheduled
/// occurrence in the **half-open** window `(after, now]` — i.e. it became due since
/// the last scheduler tick (`after`) and is due by `now`. The half-open shape is
/// what makes a periodic scheduler fire a cron **exactly once** per occurrence:
/// consecutive windows `(t0, t1]`, `(t1, t2]` never overlap, so no double-fire and
/// no gap. The window bounds are absolute instants (UTC); only the cron's *fields*
/// are interpreted in `tz`, so a DST shift moves the local fire time correctly.
///
/// `after >= now` (a zero/negative window) is never due. A cron with no further
/// occurrence (e.g. an impossible date) is treated as not due.
///
/// # Errors
/// [`CronError`] if `cron` does not parse or `tz` is unknown.
pub fn due_in_window(
    cron: &str,
    tz: Option<&str>,
    after: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<bool, CronError> {
    Ok(due_occurrence(cron, tz, after, now)?.is_some())
}

/// Like [`due_in_window`] but returns **which** occurrence fired — the cron's first
/// scheduled instant in `(after, now]` (as UTC), or `None` if not due. The instant
/// is deterministic given the window, so a multi-pod scheduler can key a single-fire
/// lock on it (the same pending occurrence → the same key on every pod, SOUL §11).
///
/// # Errors
/// [`CronError`] if `cron` does not parse or `tz` is unknown.
pub fn due_occurrence(
    cron: &str,
    tz: Option<&str>,
    after: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>, CronError> {
    if after >= now {
        return Ok(None);
    }
    let zone = resolve_tz(cron, tz)?;
    let schedule = parse(cron)?;
    // croner interprets the cron fields in the timezone of the passed DateTime, so
    // evaluate from `after` *in `zone`*; compare the next occurrence back as an
    // instant (UTC) against `now`.
    let after_local = after.with_timezone(&zone);
    match schedule.find_next_occurrence(&after_local, false) {
        Ok(next) => {
            let next = next.with_timezone(&Utc);
            Ok((next <= now).then_some(next))
        }
        // No representable next occurrence → not due (not a hard error).
        Err(_) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap()
    }

    #[test]
    fn validate_accepts_five_field_and_rejects_garbage() {
        assert!(validate("0 9 * * *", None).is_ok());
        assert!(validate("*/5 * * * *", None).is_ok());
        assert!(validate("0 0 * * MON", None).is_ok());
        assert!(validate("0 9 * * *", Some("America/New_York")).is_ok());
        assert!(validate("not a cron", None).is_err());
        assert!(validate("99 99 99 99 99", None).is_err());
        assert!(validate("", None).is_err());
        // A valid cron with an unknown timezone is rejected.
        assert!(validate("0 9 * * *", Some("Mars/Olympus_Mons")).is_err());
    }

    #[test]
    fn every_minute_fires_when_a_minute_boundary_is_crossed() {
        // Window crossing 12:01:00 → the every-minute cron is due.
        let after = t(2026, 6, 15, 12, 0, 30);
        let now = t(2026, 6, 15, 12, 1, 5);
        assert!(due_in_window("* * * * *", None, after, now).unwrap());
    }

    #[test]
    fn every_minute_does_not_fire_within_a_single_minute() {
        // Window wholly inside 12:00:xx → no minute boundary crossed → not due.
        let after = t(2026, 6, 15, 12, 0, 10);
        let now = t(2026, 6, 15, 12, 0, 50);
        assert!(!due_in_window("* * * * *", None, after, now).unwrap());
    }

    #[test]
    fn daily_nine_am_fires_only_when_its_time_is_in_the_window() {
        // A window spanning 09:00 UTC fires `0 9 * * *` (UTC).
        let across = due_in_window(
            "0 9 * * *",
            None,
            t(2026, 6, 15, 8, 59, 0),
            t(2026, 6, 15, 9, 0, 30),
        )
        .unwrap();
        assert!(across);
        // A window at 3pm does not.
        let afternoon = due_in_window(
            "0 9 * * *",
            None,
            t(2026, 6, 15, 15, 0, 0),
            t(2026, 6, 15, 15, 1, 0),
        )
        .unwrap();
        assert!(!afternoon);
    }

    #[test]
    fn cron_is_evaluated_in_the_given_timezone() {
        // `0 9 * * *` in America/New_York = 09:00 ET. On 2026-06-15 (EDT, UTC-4)
        // that is 13:00 UTC, so a 13:00-UTC window fires it and a 09:00-UTC one
        // does NOT (which is when the *UTC* interpretation would fire).
        let ny = Some("America/New_York");
        assert!(
            due_in_window(
                "0 9 * * *",
                ny,
                t(2026, 6, 15, 12, 59, 0),
                t(2026, 6, 15, 13, 0, 30)
            )
            .unwrap(),
            "9am ET == 13:00 UTC in June (EDT)"
        );
        assert!(
            !due_in_window(
                "0 9 * * *",
                ny,
                t(2026, 6, 15, 8, 59, 0),
                t(2026, 6, 15, 9, 0, 30)
            )
            .unwrap(),
            "9am UTC is NOT 9am ET"
        );
        // The same cron with no tz (UTC) fires at the 09:00-UTC window instead.
        assert!(due_in_window(
            "0 9 * * *",
            None,
            t(2026, 6, 15, 8, 59, 0),
            t(2026, 6, 15, 9, 0, 30)
        )
        .unwrap());
    }

    #[test]
    fn zero_or_negative_window_is_never_due() {
        let now = t(2026, 6, 15, 9, 0, 0);
        assert!(!due_in_window("* * * * *", None, now, now).unwrap());
        assert!(!due_in_window("* * * * *", None, now, t(2026, 6, 15, 8, 0, 0)).unwrap());
    }

    #[test]
    fn a_bad_cron_or_timezone_is_an_error_not_a_panic() {
        assert!(due_in_window(
            "nonsense",
            None,
            t(2026, 6, 15, 0, 0, 0),
            t(2026, 6, 15, 1, 0, 0)
        )
        .is_err());
        assert!(due_in_window(
            "0 9 * * *",
            Some("Nowhere/Land"),
            t(2026, 6, 15, 0, 0, 0),
            t(2026, 6, 15, 23, 0, 0)
        )
        .is_err());
    }
}
