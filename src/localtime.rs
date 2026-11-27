//! Local time from the TZ environment variable (e.g. Europe/Amsterdam), used for
//! the daily job times, dates shown in the UI and log timestamps. The time zone
//! database is built into the binary, so it works in the distroless image.

use jiff::ZonedDateTime as Zoned;
use jiff::tz::TimeZone;

/// The configured zone: TZ if set and valid, otherwise UTC.
pub fn zone() -> TimeZone {
    match std::env::var("TZ") {
        Ok(name) if !name.trim().is_empty() => TimeZone::get(name.trim()).unwrap_or(TimeZone::UTC),
        _ => TimeZone::UTC,
    }
}

pub fn now() -> Zoned {
    Zoned::now().with_time_zone(zone())
}

/// "2026-11-27" for a unix timestamp, in local time.
pub fn date(ts: i64) -> String {
    jiff::Timestamp::from_second(ts)
        .map(|t| t.to_zoned_datetime(zone()).strftime("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

/// Local time of day "HH:MM" for a Unix timestamp.
pub fn time_of(ts: i64) -> String {
    jiff::Timestamp::from_second(ts)
        .map(|t| t.to_zoned_datetime(zone()).strftime("%H:%M").to_string())
        .unwrap_or_default()
}

/// Today's local date, "2026-11-27".
pub fn today() -> String {
    now().strftime("%Y-%m-%d").to_string()
}

/// Current local time as "HH:MM".
pub fn hh_mm() -> String {
    now().strftime("%H:%M").to_string()
}

/// tracing timestamps in local time.
pub struct LogTimer;

impl tracing_subscriber::fmt::time::FormatTime for LogTimer {
    fn format_time(&self, w: &mut tracing_subscriber::fmt::format::Writer<'_>) -> std::fmt::Result {
        write!(w, "{}", now().strftime("%Y-%m-%d %H:%M:%S"))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn dates_in_zone() {
        // 2026-11-27 23:30 UTC is already the 28th in Amsterdam.
        let ts = 1_795_822_200;
        let utc = jiff::Timestamp::from_second(ts)
            .unwrap()
            .to_zoned_datetime(jiff::tz::TimeZone::UTC);
        let ams = jiff::Timestamp::from_second(ts)
            .unwrap()
            .to_zoned_datetime(jiff::tz::TimeZone::get("Europe/Amsterdam").unwrap());
        assert_eq!(utc.strftime("%Y-%m-%d").to_string(), "2026-11-27");
        assert_eq!(ams.strftime("%Y-%m-%d").to_string(), "2026-11-28");
    }
}
