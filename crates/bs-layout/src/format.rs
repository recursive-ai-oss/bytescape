//! Text for sizes, counts and names.

use std::borrow::Cow;

/// Formats a byte count with SI units (1 kB is 1000 bytes) and one decimal: `512 B`,
/// `1.2 kB`, `41.2 GB`. Values that would round up to 1000 move to the next
/// unit, so `999.95 kB` reads `1.0 MB`. Halves round up.
pub fn format_size(bytes: u64) -> String {
    let mut out = String::with_capacity(8);
    write_size(&mut out, bytes);
    out
}

/// Appends [`format_size`]'s text to `out`.
pub(crate) fn write_size(out: &mut String, bytes: u64) {
    use std::fmt::Write as _;
    const UNITS: [&str; 7] = ["B", "kB", "MB", "GB", "TB", "PB", "EB"];
    if bytes < 1000 {
        let _ = write!(out, "{bytes} B");
        return;
    }
    // Whole tenths of the unit, rounded half up, in integers so the result
    // is the same everywhere.
    let mut unit = 1u128;
    let mut index = 1;
    let mut tenths;
    loop {
        unit *= 1000;
        tenths = (u128::from(bytes) * 10 + unit / 2) / unit;
        if tenths < 10_000 || index == UNITS.len() - 1 {
            break;
        }
        index += 1;
    }
    let _ = write!(out, "{}.{} {}", tenths / 10, tenths % 10, UNITS[index]);
}

/// Formats a count with thousands separators: `3,912`.
pub fn format_count(count: u64) -> String {
    let digits = count.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Converts a raw name for display. Bytes that are not valid UTF-8 and
/// control characters (a newline in a file name, say) become U+FFFD so a
/// label is always one line of printable text.
pub fn display_name(name: &[u8]) -> Box<str> {
    let text = String::from_utf8_lossy(name);
    if text.chars().any(char::is_control) {
        text.chars()
            .map(|c| if c.is_control() { '\u{FFFD}' } else { c })
            .collect::<String>()
            .into_boxed_str()
    } else {
        match text {
            Cow::Borrowed(s) => s.into(),
            Cow::Owned(s) => s.into_boxed_str(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_use_decimal_units() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(999), "999 B");
        assert_eq!(format_size(1000), "1.0 kB");
        assert_eq!(format_size(1_234_567), "1.2 MB");
        assert_eq!(format_size(41_200_000_000), "41.2 GB");
    }

    #[test]
    fn sizes_never_print_a_thousand_of_a_unit() {
        assert_eq!(format_size(999_949), "999.9 kB");
        assert_eq!(format_size(999_950), "1.0 MB");
        assert_eq!(format_size(999_999_999), "1.0 GB");
        assert!(format_size(u64::MAX).ends_with("EB"));
    }

    #[test]
    fn counts_get_separators() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1000), "1,000");
        assert_eq!(format_count(3912), "3,912");
        assert_eq!(format_count(1_000_000), "1,000,000");
    }

    #[test]
    fn names_become_one_printable_line() {
        assert_eq!(&*display_name(b"plain.txt"), "plain.txt");
        assert_eq!(&*display_name(b"a\nb"), "a\u{FFFD}b");
        assert_eq!(&*display_name(b"bad\xFFbytes"), "bad\u{FFFD}bytes");
        assert_eq!(&*display_name("Übersicht".as_bytes()), "Übersicht");
    }
}
