//! The pre-filled GitHub issue link.
//!
//! Bytescape never sends anything: this builds a link, and the caller opens
//! it in the user's browser. The report in the link is always the one with
//! paths hidden.

use super::Report;

/// Where a new issue is started.
pub const ISSUE_URL: &str = "https://github.com/recursive-ai-oss/bytescape/issues/new";

/// The longest link built, in bytes. GitHub turns away request lines of about
/// 8,000 bytes and browsers accept far more, so this leaves room under the
/// stricter limit.
pub const MAX_URL_BYTES: usize = 7_000;

/// What replaces the end of a report that did not fit in the link.
pub const TRUNCATION_MARKER: &str = "\n[... report shortened to fit the link ...]\n";

/// The most characters in the issue title.
const MAX_TITLE_CHARS: usize = 100;

const INTRO: &str = "Bytescape closed unexpectedly and wrote this report on my computer. \
Paths in it are hidden.\n\nWhat I was doing when it happened:\n\n\n";
const FENCE_OPEN: &str = "```text\n";
const FENCE_CLOSE: &str = "\n```\n";

/// A link that opens GitHub's new-issue page with the report filled in. The
/// text is always the redacted one, percent-encoded, and the link is at most
/// [`MAX_URL_BYTES`] long: a longer report is cut at the end (the backtrace
/// comes last) and the cut is marked with [`TRUNCATION_MARKER`].
pub fn github_issue_url(report: &Report) -> String {
    let program = report
        .render(true)
        .lines()
        .find_map(|line| line.strip_prefix("Program: ").map(str::to_owned));
    let title = match program {
        Some(program) => format!("Crash report: {program}"),
        None => "Crash report".to_owned(),
    };
    build_url(&title, &report.render(false), MAX_URL_BYTES)
}

/// How many bytes `c` takes once percent-encoded.
fn encoded_len(c: char) -> usize {
    if is_unreserved(c) {
        1
    } else {
        c.len_utf8() * 3
    }
}

fn is_unreserved(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~')
}

/// Appends `text` percent-encoded (UTF-8 bytes, upper-case hexadecimal).
fn encode_into(out: &mut String, text: &str) {
    for c in text.chars() {
        if is_unreserved(c) {
            out.push(c);
        } else {
            let mut buffer = [0u8; 4];
            for byte in c.encode_utf8(&mut buffer).bytes() {
                out.push('%');
                out.push(
                    char::from_digit(u32::from(byte >> 4), 16)
                        .map_or('0', |d| d.to_ascii_uppercase()),
                );
                out.push(
                    char::from_digit(u32::from(byte & 15), 16)
                        .map_or('0', |d| d.to_ascii_uppercase()),
                );
            }
        }
    }
}

fn encoded_len_of(text: &str) -> usize {
    text.chars().map(encoded_len).sum()
}

/// Builds the link from a title and an already redacted report.
pub(crate) fn build_url(title: &str, redacted_report: &str, max_bytes: usize) -> String {
    let title: String = title
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_TITLE_CHARS)
        .collect();
    let mut url = String::with_capacity(max_bytes);
    url.push_str(ISSUE_URL);
    url.push_str("?title=");
    encode_into(&mut url, &title);
    url.push_str("&body=");

    // A fence inside the report would end the code block early.
    let report = redacted_report.replace("```", "'''");
    let fixed = encoded_len_of(INTRO) + encoded_len_of(FENCE_OPEN) + encoded_len_of(FENCE_CLOSE);
    let whole = encoded_len_of(&report);
    let room = max_bytes.saturating_sub(url.len()).saturating_sub(fixed);

    encode_into(&mut url, INTRO);
    encode_into(&mut url, FENCE_OPEN);
    if whole <= room {
        encode_into(&mut url, &report);
    } else {
        let mut budget = room.saturating_sub(encoded_len_of(TRUNCATION_MARKER));
        let mut kept = String::new();
        for c in report.chars() {
            let cost = encoded_len(c);
            if cost > budget {
                break;
            }
            budget -= cost;
            kept.push(c);
        }
        encode_into(&mut url, &kept);
        encode_into(&mut url, TRUNCATION_MARKER);
    }
    encode_into(&mut url, FENCE_CLOSE);
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(encoded: &str) -> String {
        let bytes = encoded.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap();
                out.push(u8::from_str_radix(hex, 16).unwrap());
                i += 3;
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        String::from_utf8(out).unwrap()
    }

    fn parts(url: &str) -> (String, String) {
        let rest = url.strip_prefix(&format!("{ISSUE_URL}?title=")).unwrap();
        let (title, body) = rest.split_once("&body=").unwrap();
        (decode(title), decode(body))
    }

    #[test]
    fn the_text_is_percent_encoded() {
        let mut out = String::new();
        encode_into(&mut out, "a b&c=d#e%f+g/h\n\u{e9}\u{20ac}\u{1f600}-._~Z9");
        assert_eq!(
            out,
            "a%20b%26c%3Dd%23e%25f%2Bg%2Fh%0A%C3%A9%E2%82%AC%F0%9F%98%80-._~Z9"
        );
        assert_eq!(encoded_len_of("a b\u{20ac}"), 1 + 3 + 1 + 9);
    }

    #[test]
    fn a_short_report_is_in_the_link_whole() {
        let url = build_url(
            "Crash report: bytescape 0.1.0",
            "line one\nline & two\n",
            MAX_URL_BYTES,
        );
        assert!(url.starts_with("https://github.com/recursive-ai-oss/bytescape/issues/new?title=Crash%20report%3A%20bytescape%200.1.0&body="));
        let (title, body) = parts(&url);
        assert_eq!(title, "Crash report: bytescape 0.1.0");
        assert!(body.starts_with(INTRO));
        assert!(body.contains("```text\nline one\nline & two\n\n```\n"));
        assert!(!body.contains(TRUNCATION_MARKER));
        // Nothing that would end the link early or start another field.
        assert!(!url.contains(' ') && !url.contains('\n') && !url.contains('#'));
        assert_eq!(url.matches('&').count(), 1);
        assert_eq!(url.matches('?').count(), 1);
    }

    #[test]
    fn a_long_report_is_cut_at_the_cap_and_marked() {
        let report: String = (0..5000)
            .map(|n| format!("frame {n} \u{e9} & more\n"))
            .collect();
        let url = build_url("Crash report", &report, MAX_URL_BYTES);
        assert!(url.len() <= MAX_URL_BYTES, "{}", url.len());
        // Close to the cap, not far under it.
        assert!(url.len() > MAX_URL_BYTES - 40, "{}", url.len());
        let (_, body) = parts(&url);
        assert!(body.contains(TRUNCATION_MARKER));
        assert!(body.ends_with("\n```\n"));
        // What is kept is the start of the report.
        assert!(body.contains("frame 0 \u{e9} & more\nframe 1 "));
        assert!(!body.contains("frame 4999"));
    }

    #[test]
    fn the_cap_holds_for_every_character_width() {
        for text in [
            "x".repeat(20_000),
            "\u{e9}".repeat(20_000),
            "\u{20ac}".repeat(20_000),
            "\u{1f600}".repeat(20_000),
            "\n".repeat(20_000),
        ] {
            for max in [1000, 2500, MAX_URL_BYTES] {
                let url = build_url("t", &text, max);
                assert!(url.len() <= max, "{} > {max}", url.len());
                // Always valid percent-encoding of valid text.
                let (_, body) = parts(&url);
                assert!(body.ends_with("\n```\n"));
            }
        }
    }

    #[test]
    fn a_fence_in_the_report_cannot_end_the_code_block() {
        let url = build_url("t", "a ``` b\n```\nc", MAX_URL_BYTES);
        let (_, body) = parts(&url);
        assert_eq!(body.matches("```").count(), 2, "{body}");
        assert!(body.contains("a ''' b"));
    }

    #[test]
    fn the_title_is_one_short_line() {
        let url = build_url(
            &format!("Crash\nreport {}", "x".repeat(500)),
            "r",
            MAX_URL_BYTES,
        );
        let (title, _) = parts(&url);
        assert_eq!(title.chars().count(), MAX_TITLE_CHARS);
        assert!(!title.contains('\n'));
    }
}
