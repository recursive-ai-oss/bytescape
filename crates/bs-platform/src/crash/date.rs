//! Calendar dates from Unix seconds, in UTC. The crash hook formats times with
//! this, so nothing here may panic or allocate.

// Every value is clamped to a range where none of these operations can
// overflow, so plain arithmetic is fine in this file.
#![allow(clippy::arithmetic_side_effects)]

/// Seconds either side of the epoch that are accepted: about 34,000 years,
/// far beyond any real clock and small enough that no step below overflows.
const LIMIT: i64 = 1 << 40;

/// A moment in UTC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DateTime {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

/// Splits Unix seconds into a calendar date and time. Values far outside any
/// real clock are clamped.
pub(crate) fn from_unix(seconds: i64) -> DateTime {
    let seconds = seconds.clamp(-LIMIT, LIMIT);
    let days = seconds.div_euclid(86_400);
    let in_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    DateTime {
        year,
        month,
        day,
        hour: (in_day / 3600) as u32,
        minute: (in_day % 3600 / 60) as u32,
        second: (in_day % 60) as u32,
    }
}

/// Days since 1970-01-01 to a proleptic Gregorian date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

pub(crate) fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

pub(crate) fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if is_leap_year(year) => 29,
        _ => 28,
    }
}

/// Whole calendar months from `from` to `to`, both Unix seconds. A month is
/// complete on the same day of the month, or on the last day of a shorter
/// month (from the 31st of August, February's last day completes six months).
/// Zero when `to` is not after `from`.
pub(crate) fn whole_months_between(from: i64, to: i64) -> u64 {
    if to <= from {
        return 0;
    }
    let (a, b) = (from_unix(from), from_unix(to));
    let months = (b.year - a.year) * 12 + i64::from(b.month) - i64::from(a.month);
    let due_day = a.day.min(days_in_month(b.year, b.month));
    let months = if b.day < due_day { months - 1 } else { months };
    months.max(0) as u64
}

/// `2026-10-14 19:50:00`.
pub(crate) fn format_date_time(seconds: i64) -> String {
    let t = from_unix(seconds);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        t.year, t.month, t.day, t.hour, t.minute, t.second
    )
}

/// `2026-10-14`.
pub(crate) fn format_date(seconds: i64) -> String {
    let t = from_unix(seconds);
    format!("{:04}-{:02}-{:02}", t.year, t.month, t.day)
}

/// `20261014T195000Z`, for file names that sort by time.
pub(crate) fn format_compact(seconds: i64) -> String {
    let t = from_unix(seconds);
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        t.year, t.month, t.day, t.hour, t.minute, t.second
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_moments() {
        assert_eq!(format_date_time(0), "1970-01-01 00:00:00");
        assert_eq!(format_date_time(1_705_320_000), "2024-01-15 12:00:00");
        // A leap day and the second before it ends.
        assert_eq!(format_date_time(1_709_251_199), "2024-02-29 23:59:59");
        assert_eq!(format_date_time(1_709_251_200), "2024-03-01 00:00:00");
        assert_eq!(format_date_time(-1), "1969-12-31 23:59:59");
        assert_eq!(format_compact(1_705_320_000), "20240115T120000Z");
        assert_eq!(format_date(1_705_320_000), "2024-01-15");
    }

    #[test]
    fn extreme_values_are_clamped_and_do_not_panic() {
        for seconds in [i64::MIN, i64::MAX, -LIMIT - 1, LIMIT + 1] {
            let t = from_unix(seconds);
            assert!((1..=12).contains(&t.month));
            assert!((1..=31).contains(&t.day));
        }
    }

    #[test]
    fn month_lengths() {
        assert_eq!(days_in_month(2024, 2), 29);
        assert_eq!(days_in_month(2023, 2), 28);
        assert_eq!(days_in_month(1900, 2), 28);
        assert_eq!(days_in_month(2000, 2), 29);
        assert_eq!(days_in_month(2024, 4), 30);
        assert_eq!(days_in_month(2024, 12), 31);
    }

    /// Midnight UTC of a date, found without this module's own conversion.
    fn at(year: i64, month: u32, day: u32) -> i64 {
        // Days from civil, the standard inverse.
        let y = if month <= 2 { year - 1 } else { year };
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let mp = i64::from((month + 9) % 12);
        let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        (era * 146_097 + doe - 719_468) * 86_400
    }

    #[test]
    fn the_inverse_used_by_the_tests_agrees() {
        assert_eq!(at(1970, 1, 1), 0);
        assert_eq!(at(2024, 1, 15) + 12 * 3600, 1_705_320_000);
        assert_eq!(format_date(at(2031, 7, 9)), "2031-07-09");
    }

    #[test]
    fn months_are_counted_by_the_calendar() {
        let months = |a: (i64, u32, u32), b: (i64, u32, u32)| {
            whole_months_between(at(a.0, a.1, a.2), at(b.0, b.1, b.2))
        };
        assert_eq!(months((2026, 1, 15), (2026, 7, 15)), 6);
        assert_eq!(months((2026, 1, 15), (2026, 7, 14)), 5);
        assert_eq!(months((2025, 8, 20), (2026, 10, 20)), 14);
        assert_eq!(months((2025, 8, 20), (2026, 10, 19)), 13);
        // Month lengths differ, so 30 days is not a month.
        assert_eq!(months((2026, 1, 1), (2026, 1, 31)), 0);
        assert_eq!(months((2026, 1, 1), (2026, 2, 1)), 1);
        // The 31st is completed by the last day of a shorter month.
        assert_eq!(months((2025, 8, 31), (2026, 2, 28)), 6);
        assert_eq!(months((2025, 8, 31), (2026, 2, 27)), 5);
        assert_eq!(months((2024, 8, 31), (2025, 2, 28)), 6);
        assert_eq!(months((2023, 8, 31), (2024, 2, 29)), 6);
        assert_eq!(months((2023, 8, 31), (2024, 2, 28)), 5);
        // Across a year, a leap day.
        assert_eq!(months((2024, 2, 29), (2025, 2, 28)), 12);
        assert_eq!(months((2024, 2, 29), (2025, 2, 27)), 11);
    }

    #[test]
    fn no_time_has_no_months() {
        assert_eq!(whole_months_between(100, 100), 0);
        assert_eq!(whole_months_between(100, 0), 0);
        assert!(whole_months_between(i64::MIN, i64::MAX) > 0);
    }
}
