//! Dependency-free UTC formatting for what people and small models read.

/// Proleptic Gregorian civil date from days since 1970-01-01 (Howard
/// Hinnant's `civil_from_days`). No time zone database: the harness reports
/// UTC and says so.
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

const WEEKDAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

/// Wall clock in unix milliseconds, or `0` if the system clock is before the
/// epoch.
///
/// The engine takes its time from an injected clock, which is what makes a
/// turn replayable. A store does not have one: it is constructed before any
/// engine and is shared by several. So the one reading a store needs — "how
/// long ago was this fact last used", for M9 T3.1's decay — comes from here,
/// and the decay is deliberately the only thing in a store that reads it.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `2026-09-02 10:41:28 UTC (Wednesday)`.
pub fn format_utc(unix_ms: u64) -> String {
    let secs = (unix_ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let weekday = WEEKDAYS[(days + 4).rem_euclid(7) as usize];
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC ({weekday})",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// `10:41 UTC` — for inline markers such as "(was X until 10:41 UTC)".
pub fn format_utc_short(unix_ms: u64) -> String {
    let secs = (unix_ms / 1000) as i64;
    let rem = secs.rem_euclid(86_400);
    format!("{:02}:{:02} UTC", rem / 3600, (rem % 3600) / 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_known_instants() {
        assert_eq!(
            format_utc(1_788_345_688_203),
            "2026-09-02 10:41:28 UTC (Wednesday)"
        );
        assert_eq!(format_utc(0), "1970-01-01 00:00:00 UTC (Thursday)");
        assert_eq!(
            format_utc(1_709_164_800_000),
            "2024-02-29 00:00:00 UTC (Thursday)"
        );
        assert_eq!(format_utc_short(1_788_345_688_203), "10:41 UTC");
    }
}
