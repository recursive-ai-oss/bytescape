//! The words and facts behind two small screens: the note shown after a
//! crash, and the About box.

use bs_platform::crash::{Report, github_issue_url};

/// What the banner says after a crash.
pub const CRASH_WORDS: &str = "Bytescape closed unexpectedly last time.";

/// Where releases are listed. "Check for updates" opens this in the browser;
/// Bytescape itself never asks the network anything.
pub const RELEASES_URL: &str = "https://github.com/recursive-ai-oss/bytescape/releases";

/// Longest report line shown, in characters.
const LINE_LIMIT: usize = 240;
/// Most report lines kept.
const LINES_LIMIT: usize = 2_000;

/// A crash report left by an earlier run, ready to show: the text with paths
/// hidden and with paths, and the link that starts a GitHub issue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrashNotice {
    hidden: String,
    full: String,
    url: String,
}

impl CrashNotice {
    pub fn new(hidden: String, full: String, issue_url: String) -> Self {
        Self {
            hidden,
            full,
            url: issue_url,
        }
    }

    /// Reads what the notice needs from a report.
    pub fn from_report(report: &Report) -> Self {
        Self::new(
            report.render(false),
            report.render(true),
            github_issue_url(report),
        )
    }

    /// The report as text, with paths hidden unless `show_paths`.
    pub fn text(&self, show_paths: bool) -> &str {
        if show_paths { &self.full } else { &self.hidden }
    }

    /// The link that opens a GitHub issue with the hidden-path report in it.
    pub fn issue_url(&self) -> &str {
        &self.url
    }

    /// The report split into lines for the viewer. A line is shortened to a
    /// fixed number of characters and the number of lines is limited, so a
    /// hostile report cannot make the viewer large.
    pub fn lines(&self, show_paths: bool) -> Vec<String> {
        self.text(show_paths)
            .lines()
            .take(LINES_LIMIT)
            .map(|l| {
                let mut line: String = l
                    .chars()
                    .take(LINE_LIMIT)
                    .map(|c| if c.is_control() { ' ' } else { c })
                    .collect();
                if l.chars().count() > LINE_LIMIT {
                    line.push('…');
                }
                line
            })
            .collect()
    }
}

/// What the About box says about this build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AboutInfo {
    pub version: String,
    /// `YYYY-MM-DD`, or anything else (such as "unknown") when the build did
    /// not record a date.
    pub build_date: String,
    pub target: String,
}

impl AboutInfo {
    pub fn new(version: &str, build_date: &str, target: &str) -> Self {
        Self {
            version: version.into(),
            build_date: build_date.into(),
            target: target.into(),
        }
    }

    /// The build date as Unix seconds, when it is a real date.
    pub fn build_epoch(&self) -> Option<i64> {
        parse_date(&self.build_date)
    }

    /// "Version 0.1.0 · built 2026-10-14", or without the date when the build
    /// did not record one.
    pub fn version_line(&self) -> String {
        if self.build_epoch().is_some() {
            format!("Version {} · built {}", self.version, self.build_date)
        } else {
            format!("Version {}", self.version)
        }
    }

    /// "This version is 14 months old", when the build is old enough to say so
    /// and its date is known. `now` is Unix seconds.
    pub fn age_line(&self, now: i64) -> Option<String> {
        bs_platform::crash::build_age(self.build_epoch()?, now)
    }
}

/// Unix seconds at midnight UTC of a `YYYY-MM-DD` date.
fn parse_date(text: &str) -> Option<i64> {
    let mut parts = text.trim().split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=9999).contains(&year) {
        return None;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let longest = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    if !(1..=longest).contains(&day) {
        return None;
    }
    // Days from the civil date (proleptic Gregorian calendar).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146_097 + doe - 719_468) * 86_400)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_parse_to_midnight_utc() {
        assert_eq!(parse_date("1970-01-01"), Some(0));
        assert_eq!(parse_date("2024-03-05"), Some(1_709_596_800));
        assert_eq!(parse_date("2000-02-29"), Some(951_782_400));
        for bad in [
            "unknown",
            "",
            "2026-13-01",
            "2026-02-29",
            "2026-04-31",
            "2026-1",
            "2026-01-01-01",
            "0000-01-01",
            "x-y-z",
        ] {
            assert_eq!(parse_date(bad), None, "{bad}");
        }
    }

    #[test]
    fn an_unknown_build_date_gives_no_age_and_no_date() {
        let about = AboutInfo::new("0.1.0", "unknown", "x86_64-unknown-linux-gnu");
        assert_eq!(about.version_line(), "Version 0.1.0");
        assert_eq!(about.age_line(2_000_000_000), None);
    }

    #[test]
    fn report_lines_are_shortened_and_cleaned() {
        let long = "x".repeat(1000);
        let n = CrashNotice::new(
            format!("one\u{0007}bell\n{long}\n"),
            String::new(),
            String::new(),
        );
        let lines = n.lines(false);
        assert_eq!(lines[0], "one bell");
        assert_eq!(lines[1].chars().count(), LINE_LIMIT + 1);
        assert!(lines[1].ends_with('…'));
        let many = CrashNotice::new("a\n".repeat(10_000), String::new(), String::new());
        assert_eq!(many.lines(false).len(), LINES_LIMIT);
    }
}
