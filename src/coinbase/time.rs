//! RFC 3339 timestamps as Coinbase sends them (`2026-11-09T12:00:00.123456Z`), without a date
//! library.

/// Milliseconds since the Unix epoch, or `None` when the text is not a UTC/offset RFC 3339 time.
pub fn parse_rfc3339_ms(text: &str) -> Option<i64> {
    let text = text.trim();
    let (date, time) = text.split_once(['T', 't', ' '])?;
    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: i64 = date_parts.next()?.parse().ok()?;
    let day: i64 = date_parts.next()?.parse().ok()?;
    if date_parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Split the clock from its offset.
    let (clock, offset_secs) = if let Some(clock) = time.strip_suffix(['Z', 'z']) {
        (clock, 0_i64)
    } else {
        let at = time.rfind(['+', '-'])?;
        let (clock, offset) = time.split_at(at);
        let sign = if offset.starts_with('-') { -1 } else { 1 };
        let (hours, minutes) = offset[1..].split_once(':')?;
        (
            clock,
            sign * (hours.parse::<i64>().ok()? * 3600 + minutes.parse::<i64>().ok()? * 60),
        )
    };
    let (whole, fraction) = clock.split_once('.').unwrap_or((clock, ""));
    let mut clock_parts = whole.split(':');
    let hour: i64 = clock_parts.next()?.parse().ok()?;
    let minute: i64 = clock_parts.next()?.parse().ok()?;
    let second: i64 = clock_parts.next()?.parse().ok()?;
    if clock_parts.next().is_some() || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let millis: i64 = if fraction.is_empty() {
        0
    } else {
        if !fraction.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let padded = format!("{fraction:0<3}");
        padded[..3].parse().ok()?
    };
    let days = days_from_civil(year, month, day);
    Some(((days * 86_400 + hour * 3600 + minute * 60 + second - offset_secs) * 1000) + millis)
}

/// `2026-11-09T12:00:00.123Z` for a Unix time in milliseconds.
pub fn format_rfc3339_ms(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// The inverse of [`days_from_civil`].
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (year_of_era + era * 400 + i64::from(month <= 2), month, day)
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_zulu_fractions_and_offsets() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_ms("2000-03-01T00:00:00Z"),
            Some(951_868_800_000)
        );
        assert_eq!(
            parse_rfc3339_ms("2026-11-09T12:00:00.123456Z"),
            Some(1_794_225_600_123)
        );
        assert_eq!(
            parse_rfc3339_ms("2026-11-09T12:00:00.5Z"),
            Some(1_794_225_600_500)
        );
        assert_eq!(
            parse_rfc3339_ms("2026-11-09T14:00:00+02:00"),
            Some(1_794_225_600_000)
        );
        assert_eq!(
            parse_rfc3339_ms("2026-11-09T07:00:00-05:00"),
            Some(1_794_225_600_000)
        );
    }

    #[test]
    fn rejects_things_that_are_not_timestamps() {
        for bad in [
            "",
            "yesterday",
            "2026-13-01T00:00:00Z",
            "2026-11-09",
            "2026-11-09T25:00:00Z",
            "2026-11-09T00:00:00.xZ",
        ] {
            assert_eq!(parse_rfc3339_ms(bad), None, "{bad}");
        }
    }

    #[test]
    fn formatting_is_the_inverse_of_parsing() {
        for ms in [
            0_i64,
            951_868_800_000,
            1_709_164_800_000,
            1_794_225_600_123,
            4_102_444_799_999,
        ] {
            assert_eq!(parse_rfc3339_ms(&format_rfc3339_ms(ms)), Some(ms), "{ms}");
        }
        assert_eq!(
            format_rfc3339_ms(1_794_225_600_123),
            "2026-11-09T12:00:00.123Z"
        );
    }

    #[test]
    fn handles_leap_days_and_the_unset_time_coinbase_uses() {
        assert_eq!(
            parse_rfc3339_ms("2024-02-29T00:00:00Z"),
            Some(1_709_164_800_000)
        );
        // Coinbase writes "no end time" as the year 1.
        assert!(parse_rfc3339_ms("0001-01-01T00:00:00Z").unwrap() < 0);
    }
}
