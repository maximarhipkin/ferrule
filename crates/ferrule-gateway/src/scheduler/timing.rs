//! Cron/one-shot timing math, kept separate from `store.rs` so it can be
//! unit-tested without touching SQLite at all.
//!
//! Design note on the "missed run" / backlog policy (see `mod.rs`
//! `advance_next_run_at`): this module only ever answers "what's the next
//! occurrence strictly after this instant", never "what occurrences did we
//! miss between two instants". That asymmetry is deliberate — it's what
//! makes a long daemon outage collapse to at most one catch-up run per task
//! instead of a burst of backlogged ones.

use super::error::SchedulerError;
use chrono::{DateTime, TimeZone, Utc};
use croner::parser::{CronParser, Seconds};
use croner::Cron;

/// Parses a strict 5-field cron expression (`min hour day month weekday`,
/// no seconds field — keeps task schedules readable and matches the classic
/// crontab format users already know).
pub fn parse_cron(expr: &str) -> Result<Cron, SchedulerError> {
    CronParser::builder()
        .seconds(Seconds::Disallowed)
        .build()
        .parse(expr)
        .map_err(|e| SchedulerError::InvalidCron(expr.to_string(), e.to_string()))
}

/// Next occurrence of `cron` strictly after `after`, evaluated in `timezone`
/// (an IANA name, e.g. "Asia/Jerusalem") so DST transitions are handled by
/// `chrono_tz`'s real rules rather than a fixed UTC offset baked in at
/// creation time.
pub fn next_cron_occurrence(
    cron: &Cron,
    timezone: &str,
    after: DateTime<Utc>,
) -> Result<DateTime<Utc>, SchedulerError> {
    let tz: chrono_tz::Tz = timezone
        .parse()
        .map_err(|_| SchedulerError::InvalidTimezone(timezone.to_string()))?;
    let after_local = after.with_timezone(&tz);
    let next_local = cron
        .find_next_occurrence(&after_local, false)
        .map_err(|e| SchedulerError::CronSearch(e.to_string()))?;
    Ok(next_local.with_timezone(&Utc))
}

/// Parses a one-shot task's schedule: an RFC 3339 timestamp, e.g.
/// `2026-10-01T09:00:00+03:00`. Unlike cron tasks, a one-shot's `timezone`
/// field is not consulted here — the offset is already explicit in the
/// timestamp itself.
pub fn parse_once(schedule: &str) -> Result<DateTime<Utc>, SchedulerError> {
    DateTime::parse_from_rfc3339(schedule)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|_| SchedulerError::InvalidDatetime(schedule.to_string()))
}

/// `chrono::DateTime::from_timestamp` returns `None` only for
/// out-of-range values; unix seconds coming from our own `now_unix()` never
/// hit that, but callers still need somewhere to fall back rather than
/// panicking on a store row that (somehow) got corrupted.
pub fn from_unix(secs: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(secs, 0).single().unwrap_or_else(Utc::now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cron_next_occurrence_respects_non_utc_timezone_and_dst() {
        let cron = parse_cron("0 9 * * *").unwrap(); // daily at 09:00 local

        // Winter: Israel is UTC+2 (no DST) — 09:00 local = 07:00 UTC.
        let winter_after = Utc.with_ymd_and_hms(2026, 1, 14, 0, 0, 0).unwrap();
        let next = next_cron_occurrence(&cron, "Asia/Jerusalem", winter_after).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 1, 14, 7, 0, 0).unwrap());

        // Summer: Israel is UTC+3 (DST) — 09:00 local = 06:00 UTC. If this
        // module were doing naive fixed-offset math instead of real tz
        // lookup, this would come out wrong (still 07:00).
        let summer_after = Utc.with_ymd_and_hms(2026, 7, 14, 0, 0, 0).unwrap();
        let next = next_cron_occurrence(&cron, "Asia/Jerusalem", summer_after).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 7, 14, 6, 0, 0).unwrap());
    }

    #[test]
    fn cron_next_occurrence_is_strictly_after_not_inclusive() {
        let cron = parse_cron("*/30 * * * *").unwrap();
        let at_boundary = Utc.with_ymd_and_hms(2026, 3, 1, 12, 30, 0).unwrap();
        let next = next_cron_occurrence(&cron, "UTC", at_boundary).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 3, 1, 13, 0, 0).unwrap());
    }

    #[test]
    fn invalid_cron_expression_is_rejected() {
        let err = parse_cron("not a cron expression").unwrap_err();
        assert!(matches!(err, SchedulerError::InvalidCron(_, _)));
    }

    #[test]
    fn invalid_timezone_is_rejected() {
        let cron = parse_cron("0 9 * * *").unwrap();
        let err = next_cron_occurrence(&cron, "Not/ARealZone", Utc::now()).unwrap_err();
        assert!(matches!(err, SchedulerError::InvalidTimezone(_)));
    }

    #[test]
    fn once_parses_rfc3339_and_rejects_garbage() {
        let dt = parse_once("2026-10-01T09:00:00+03:00").unwrap();
        assert_eq!(dt, Utc.with_ymd_and_hms(2026, 10, 1, 6, 0, 0).unwrap());
        assert!(parse_once("not a timestamp").is_err());
    }
}
