//! Date and time helpers shared by parsers and output, on top of `jiff`.
use std::sync::LazyLock;
use std::time::SystemTime;

use jiff::Timestamp;
use jiff::civil::DateTime;
use jiff::tz::TimeZone;

/// The local time zone (`TZ`, else the system setting), read once per run.
static LOCAL: LazyLock<TimeZone> = LazyLock::new(TimeZone::system);

pub fn local_tz() -> &'static TimeZone {
    &LOCAL
}

/// Parse an RFC 3339 timestamp such as `2026-09-27T02:34:43.613+00:00`.
pub fn parse_rfc3339(s: &str) -> Option<Timestamp> {
    s.parse().ok()
}

/// Local date and time of Unix seconds; `None` when out of range.
pub fn local_datetime(unix: i64) -> Option<DateTime> {
    Timestamp::from_second(unix)
        .ok()
        .map(|ts| local_tz().to_datetime(ts))
}

/// The instant of a local date and time, or `None` when it falls in a DST
/// gap or repeats in a DST fold.
pub fn local_instant(dt: DateTime) -> Option<SystemTime> {
    instant_in(dt, local_tz())
}

fn instant_in(dt: DateTime, tz: &TimeZone) -> Option<SystemTime> {
    let ts = tz.to_ambiguous_timestamp(dt).unambiguous().ok()?;
    SystemTime::try_from(ts).ok()
}

/// `YYYY-MM-DD HH:MM` in local time.
pub fn format_local_minutes(time: SystemTime) -> String {
    let ts = Timestamp::try_from(time).unwrap_or(Timestamp::UNIX_EPOCH);
    local_tz()
        .to_datetime(ts)
        .strftime("%Y-%m-%d %H:%M")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_forms() {
        let utc = parse_rfc3339("2026-09-27T02:34:43.613+00:00").unwrap();
        assert_eq!(utc.as_millisecond(), 1_790_476_483_613);
        assert_eq!(parse_rfc3339("2026-09-27T02:34:43.613Z"), Some(utc));
        assert_eq!(parse_rfc3339("2026-09-27T11:34:43.613+09:00"), Some(utc));
        assert_eq!(
            parse_rfc3339("2026-09-27 02:34:43Z").map(|t| t.as_second()),
            Some(utc.as_second())
        );
        for bad in ["", "2026-09-27", "2026-09-27T02:34:43", "not a time"] {
            assert_eq!(parse_rfc3339(bad), None, "{bad}");
        }
    }

    #[test]
    fn local_times_across_dst() {
        let ny = TimeZone::get("America/New_York").unwrap();
        let at = |s: &str| instant_in(s.parse().unwrap(), &ny);
        let secs = |t: SystemTime| Timestamp::try_from(t).unwrap().as_second();
        // EST (-05:00) before the March change, EDT (-04:00) after it.
        assert_eq!(at("2026-03-07T12:00").map(secs), Some(1_772_902_800));
        assert_eq!(at("2026-03-09T12:00").map(secs), Some(1_773_072_000));
        // 02:30 is skipped in March and occurs twice in November.
        assert_eq!(at("2026-03-08T02:30"), None);
        assert_eq!(at("2026-11-01T01:30"), None);
        let fall = Timestamp::from_second(1_793_511_000).unwrap();
        assert_eq!(ny.to_datetime(fall).to_string(), "2026-11-01T01:30:00");
    }

    #[test]
    fn out_of_range_seconds_do_not_panic() {
        assert_eq!(local_datetime(i64::MAX), None);
        assert_eq!(local_datetime(i64::MIN), None);
        assert!(local_datetime(0).is_some());
    }
}
