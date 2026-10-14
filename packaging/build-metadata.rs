// Build metadata shared by the build scripts of both binaries. Each build
// script includes this file, and the xtask tests include it too, so the date
// code has one home and is tested.
//
// It exports three compile-time variables: BYTESCAPE_VERSION,
// BYTESCAPE_BUILD_DATE and BYTESCAPE_TARGET. The build date comes from
// SOURCE_DATE_EPOCH, which CI sets to the commit time (see the test job), so
// rebuilding the same commit gives the same date. Without it the date is
// "unknown", and the About box shows no build-age note for that build.

/// Converts seconds since 1970-01-01 to a `YYYY-MM-DD` date in UTC.
pub fn utc_date_from_epoch(seconds: u64) -> String {
    let days = i64::try_from(seconds / 86_400).unwrap_or(0);
    // Civil-from-days, valid for the whole proleptic Gregorian calendar.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// The build date for a `SOURCE_DATE_EPOCH` value, or "unknown".
pub fn build_date(source_date_epoch: Option<&str>) -> String {
    match source_date_epoch.and_then(|text| text.trim().parse::<u64>().ok()) {
        Some(seconds) => utc_date_from_epoch(seconds),
        None => String::from("unknown"),
    }
}

/// Prints the cargo directives for the three variables.
pub fn emit_build_metadata() {
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let target = std::env::var("TARGET").unwrap_or_default();
    let epoch = std::env::var("SOURCE_DATE_EPOCH").ok();
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    println!("cargo:rustc-env=BYTESCAPE_VERSION={version}");
    println!(
        "cargo:rustc-env=BYTESCAPE_BUILD_DATE={}",
        build_date(epoch.as_deref())
    );
    println!("cargo:rustc-env=BYTESCAPE_TARGET={target}");
}
