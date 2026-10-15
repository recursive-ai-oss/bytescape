//! The local time zone's offset from UTC, for showing a modified date as the
//! user's clock would.

#![allow(unsafe_code)]

/// The offset of local time from UTC right now, in seconds east of UTC. Zero
/// if the system cannot say.
pub fn local_utc_offset_seconds() -> i32 {
    local_utc_offset_seconds_at(unix_now())
}

/// The offset of local time from UTC at a moment, given as seconds since the
/// Unix epoch. A date in January and one in July can differ by an hour in a
/// zone with summer time, so use this for a file's modified time. Zero if the
/// system cannot say.
///
/// Each call reads the system's zone settings; a caller showing many dates
/// should keep the results it needs.
pub fn local_utc_offset_seconds_at(unix_seconds: i64) -> i32 {
    let offset = system_offset(unix_seconds);
    // No real zone is more than a day from UTC.
    offset.clamp(-24 * 3600, 24 * 3600)
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(0))
        .unwrap_or(0)
}

#[cfg(target_os = "linux")]
fn system_offset(unix_seconds: i64) -> i32 {
    // `time_t` is 64 bits on every target this builds for.
    let time = unix_seconds as libc::time_t;
    // SAFETY: `tm` is plain data that `localtime_r` fills in; `time` and `tm`
    // are valid for the call. `tzset` reads the `TZ` variable and the zone
    // database, and takes no pointers.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        tzset();
        if libc::localtime_r(&time, &mut tm).is_null() {
            return 0;
        }
        i32::try_from(tm.tm_gmtoff).unwrap_or(0)
    }
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    /// Re-reads the `TZ` variable and the zone database.
    safe fn tzset();
}

#[cfg(windows)]
fn system_offset(unix_seconds: i64) -> i32 {
    use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    // Seconds from 1601-01-01 to 1970-01-01.
    const EPOCH_GAP: i64 = 11_644_473_600;
    let Some(ticks) = unix_seconds
        .checked_add(EPOCH_GAP)
        .filter(|s| *s >= 0)
        .and_then(|s| s.checked_mul(10_000_000))
    else {
        return 0;
    };
    let ticks = ticks as u64;
    let file_time = FILETIME {
        dwLowDateTime: (ticks & 0xFFFF_FFFF) as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    // SAFETY: `SYSTEMTIME` is plain data; both calls get valid pointers to
    // values that live through the call. A null time zone means the current
    // one.
    unsafe {
        let mut utc: SYSTEMTIME = std::mem::zeroed();
        let mut local: SYSTEMTIME = std::mem::zeroed();
        if FileTimeToSystemTime(&file_time, &mut utc) == 0
            || SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) == 0
        {
            return 0;
        }
        let seconds = |t: &SYSTEMTIME| {
            civil_seconds(
                i64::from(t.wYear),
                i64::from(t.wMonth),
                i64::from(t.wDay),
                i64::from(t.wHour),
                i64::from(t.wMinute),
                i64::from(t.wSecond),
            )
        };
        i32::try_from(seconds(&local) - seconds(&utc)).unwrap_or(0)
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
fn system_offset(_: i64) -> i32 {
    0
}

/// Seconds since 1970-01-01 00:00:00 for a calendar date and time, treated as
/// if it were UTC. Used to subtract two readings of the same moment.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn civil_seconds(year: i64, month: i64, day: i64, hour: i64, minute: i64, second: i64) -> i64 {
    // Days from civil, after Howard Hinnant's algorithm.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let year_of_era = y - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    days * 86_400 + hour * 3600 + minute * 60 + second
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_dates_become_unix_seconds() {
        assert_eq!(civil_seconds(1970, 1, 1, 0, 0, 0), 0);
        assert_eq!(civil_seconds(1970, 1, 2, 0, 0, 1), 86_401);
        assert_eq!(civil_seconds(2024, 1, 15, 12, 0, 0), 1_705_320_000);
        assert_eq!(civil_seconds(2024, 7, 15, 12, 0, 0), 1_721_044_800);
        // A leap day, the end of a year, and a date before 1970.
        assert_eq!(civil_seconds(2024, 2, 29, 0, 0, 0), 1_709_164_800);
        assert_eq!(civil_seconds(2023, 12, 31, 23, 59, 59), 1_704_067_199);
        assert_eq!(civil_seconds(1969, 12, 31, 23, 59, 59), -1);
        assert_eq!(civil_seconds(2000, 3, 1, 0, 0, 0), 951_868_800);
    }

    #[test]
    fn zones_sit_on_quarter_hours_and_move_by_at_most_an_hour_a_season() {
        // Real zones are whole quarter hours from UTC, and summer time moves
        // them by an hour at most (half an hour on Lord Howe Island).
        let winter = local_utc_offset_seconds_at(1_705_320_000);
        let summer = local_utc_offset_seconds_at(1_721_044_800);
        assert_eq!(winter % 900, 0, "{winter}");
        assert_eq!(summer % 900, 0, "{summer}");
        assert!((summer - winter).abs() <= 3600);
    }

    #[cfg(windows)]
    #[test]
    fn the_offset_matches_the_zone_settings_now() {
        use windows_sys::Win32::System::SystemServices::TIME_ZONE_ID_DAYLIGHT;
        use windows_sys::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
        // SAFETY: plain data that the call fills in.
        let mut zone: TIME_ZONE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: `zone` is writable for the call.
        let id = unsafe { GetTimeZoneInformation(&mut zone) };
        let extra = if id == TIME_ZONE_ID_DAYLIGHT {
            zone.DaylightBias
        } else {
            zone.StandardBias
        };
        assert_eq!(
            local_utc_offset_seconds_at(unix_now()),
            -(zone.Bias + extra) * 60
        );
    }

    #[test]
    fn the_offset_is_bounded_and_does_not_panic() {
        for t in [i64::MIN, -1, 0, 1_705_320_000, i64::MAX] {
            let offset = local_utc_offset_seconds_at(t);
            assert!(offset.abs() <= 24 * 3600, "{t}: {offset}");
        }
        let _ = local_utc_offset_seconds();
    }

    /// Runs `check` in a new copy of this test program with `TZ` set, because
    /// the time zone is read from the process environment and tests share it.
    #[cfg(target_os = "linux")]
    fn with_zone(zone: &str, check: &str) {
        let exe = std::env::current_exe().unwrap();
        let output = std::process::Command::new(exe)
            .args(["--exact", check, "--nocapture", "--test-threads=1"])
            .env("TZ", zone)
            .env("BS_CLOCK_INNER", "1")
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && text.contains("1 passed"),
            "{zone}: {text}{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn summer_time_changes_the_offset_by_the_date() {
        with_zone(
            "EST5EDT,M3.2.0,M11.1.0",
            "clock::tests::inner_new_york_zone",
        );
        with_zone("UTC0", "clock::tests::inner_utc_zone");
        with_zone("JST-9", "clock::tests::inner_tokyo_zone");
        with_zone("IST-5:30", "clock::tests::inner_half_hour_zone");
    }

    #[cfg(target_os = "linux")]
    fn inner() -> bool {
        // Only meaningful in the copy started by `with_zone`.
        std::env::var_os("BS_CLOCK_INNER").is_some()
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn inner_new_york_zone() {
        if inner() {
            assert_eq!(local_utc_offset_seconds_at(1_705_320_000), -5 * 3600);
            assert_eq!(local_utc_offset_seconds_at(1_721_044_800), -4 * 3600);
            // The spring change is at 07:00 UTC on 2024-03-10.
            assert_eq!(local_utc_offset_seconds_at(1_710_054_000 - 1), -5 * 3600);
            assert_eq!(local_utc_offset_seconds_at(1_710_054_000), -4 * 3600);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn inner_utc_zone() {
        if inner() {
            assert_eq!(local_utc_offset_seconds_at(1_705_320_000), 0);
            assert_eq!(local_utc_offset_seconds(), 0);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn inner_tokyo_zone() {
        if inner() {
            assert_eq!(local_utc_offset_seconds_at(0), 9 * 3600);
            assert_eq!(local_utc_offset_seconds_at(-86_400 * 365), 9 * 3600);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn inner_half_hour_zone() {
        if inner() {
            assert_eq!(local_utc_offset_seconds_at(1_705_320_000), 19_800);
        }
    }
}
