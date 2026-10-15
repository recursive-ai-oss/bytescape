//! The text formats of the freedesktop.org Trash: `.trashinfo` records and the
//! `directorysizes` cache. Pure functions, so they are tested on every system.

/// The longest `.trashinfo` file that is read.
pub(crate) const MAX_INFO_BYTES: usize = 64 * 1024;
/// The longest `directorysizes` file that is read.
pub(crate) const MAX_SIZES_BYTES: usize = 64 * 1024 * 1024;

fn is_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/')
}

/// Percent-encodes a path. Every byte except letters, digits, `-_.~` and the
/// separator is written as `%XX`, so names with spaces, newlines or bytes that
/// are not UTF-8 survive the round trip.
pub(crate) fn encode(path: &[u8]) -> String {
    let mut out = String::with_capacity(path.len());
    for &byte in path {
        if is_unreserved(byte) {
            out.push(char::from(byte));
        } else {
            out.push('%');
            for nibble in [byte >> 4, byte & 15] {
                let digit = char::from_digit(u32::from(nibble), 16).unwrap_or('0');
                out.push(digit.to_ascii_uppercase());
            }
        }
    }
    out
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Reverses [`encode`]. `None` for a `%` that is not followed by two hex
/// digits.
pub(crate) fn decode(text: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len());
    let mut index = 0;
    while index < text.len() {
        if text[index] == b'%' {
            let high = hex(*text.get(index + 1)?)?;
            let low = hex(*text.get(index + 2)?)?;
            out.push(high << 4 | low);
            index += 3;
        } else {
            out.push(text[index]);
            index += 1;
        }
    }
    Some(out)
}

/// The contents of a `.trashinfo` file. `date` is local time as
/// `YYYY-MM-DDThh:mm:ss`.
pub(crate) fn format_info(path: &[u8], date: &str) -> String {
    format!("[Trash Info]\nPath={}\nDeletionDate={date}\n", encode(path))
}

/// The original path recorded in a `.trashinfo` file. `None` when the file is
/// too large, has no `[Trash Info]` group or no readable `Path` key.
pub(crate) fn parse_info_path(contents: &[u8]) -> Option<Vec<u8>> {
    if contents.len() > MAX_INFO_BYTES {
        return None;
    }
    let mut in_group = false;
    for line in contents.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.starts_with(b"[") {
            in_group = line == b"[Trash Info]";
        } else if in_group && let Some(value) = line.strip_prefix(b"Path=") {
            return decode(value);
        }
    }
    None
}

/// `YYYY-MM-DDThh:mm:ss` from broken-down local time.
pub(crate) fn format_date(
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    min: u32,
    sec: u32,
) -> String {
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}")
}

/// One line of `directorysizes`: size in bytes, the modification time of the
/// matching `.trashinfo` file in whole seconds, and the encoded name.
fn sizes_line(size: u64, mtime: u64, name: &[u8]) -> Vec<u8> {
    format!("{size} {mtime} {}\n", encode(name)).into_bytes()
}

/// Whether a `directorysizes` line is about `name` (an encoded name is the
/// third field).
fn line_is_for(line: &[u8], encoded: &[u8]) -> bool {
    let mut fields = line.splitn(3, |&b| b == b' ');
    fields.next();
    fields.next();
    fields.next().is_some_and(|n| n == encoded)
}

/// `existing` with the line for `name` replaced or added. Lines about other
/// names are kept as they are, including ones this code cannot read.
pub(crate) fn sizes_with(existing: &[u8], size: u64, mtime: u64, name: &[u8]) -> Vec<u8> {
    let mut out = sizes_without(existing, name);
    out.extend_from_slice(&sizes_line(size, mtime, name));
    out
}

/// `existing` without the line for `name`.
pub(crate) fn sizes_without(existing: &[u8], name: &[u8]) -> Vec<u8> {
    let encoded = encode(name).into_bytes();
    let mut out = Vec::with_capacity(existing.len());
    for line in existing.split_inclusive(|&b| b == b'\n') {
        let bare = line.strip_suffix(b"\n").unwrap_or(line);
        if !line_is_for(bare, &encoded) {
            out.extend_from_slice(line);
            if !line.ends_with(b"\n") {
                out.push(b'\n');
            }
        }
    }
    out
}

/// The name to try for the `attempt`-th time: the name itself first, then
/// `name.2`, `name.3`, and so on.
pub(crate) fn candidate_name(name: &[u8], attempt: u32) -> Vec<u8> {
    if attempt <= 1 {
        return name.to_vec();
    }
    let mut out = name.to_vec();
    out.extend_from_slice(format!(".{attempt}").as_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn plain_paths_stay_readable() {
        assert_eq!(encode(b"/home/a/b-c_d.txt"), "/home/a/b-c_d.txt");
    }

    #[test]
    fn special_bytes_are_escaped() {
        assert_eq!(encode(b"a b"), "a%20b");
        assert_eq!(encode(b"100%"), "100%25");
        assert_eq!(encode(b"line\nbreak"), "line%0Abreak");
        assert_eq!(encode(&[0xff, 0xfe]), "%FF%FE");
        assert_eq!(encode("é".as_bytes()), "%C3%A9");
    }

    #[test]
    fn decoding_rejects_broken_escapes() {
        for bad in ["%", "%4", "%zz", "a%2", "%g1"] {
            assert_eq!(decode(bad.as_bytes()), None, "{bad}");
        }
        assert_eq!(decode(b"a%20b%2f"), Some(b"a b/".to_vec()));
    }

    proptest! {
        #[test]
        fn encode_then_decode_is_the_identity(bytes in proptest::collection::vec(any::<u8>(), 0..300)) {
            let text = encode(&bytes);
            prop_assert!(text.is_ascii());
            prop_assert!(!text.contains('\n') && !text.contains('\r'));
            prop_assert_eq!(decode(text.as_bytes()), Some(bytes));
        }

        #[test]
        fn decoding_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..300)) {
            let _ = decode(&bytes);
            let _ = parse_info_path(&bytes);
        }
    }

    #[test]
    fn an_info_file_round_trips() {
        let text = format_info(b"/home/me/My Files/a\nb", "2026-10-14T12:30:05");
        assert_eq!(
            text,
            "[Trash Info]\nPath=/home/me/My%20Files/a%0Ab\nDeletionDate=2026-10-14T12:30:05\n"
        );
        assert_eq!(
            parse_info_path(text.as_bytes()),
            Some(b"/home/me/My Files/a\nb".to_vec())
        );
    }

    #[test]
    fn info_files_from_other_programs_are_read() {
        // Keys in another order, other groups, CRLF endings.
        let text = b"[Other]\r\nPath=/wrong\r\n[Trash Info]\r\nDeletionDate=2020-01-01T00:00:00\r\nPath=docs/a%20b\r\n";
        assert_eq!(parse_info_path(text), Some(b"docs/a b".to_vec()));
        assert_eq!(parse_info_path(b"Path=/x\n"), None);
        assert_eq!(parse_info_path(b"[Trash Info]\nPath=%zz\n"), None);
        assert_eq!(parse_info_path(b"[Trash Info]\n"), None);
        let huge = vec![b'a'; MAX_INFO_BYTES + 1];
        assert_eq!(parse_info_path(&huge), None);
    }

    #[test]
    fn dates_are_zero_padded() {
        assert_eq!(format_date(2026, 1, 2, 3, 4, 5), "2026-01-02T03:04:05");
    }

    #[test]
    fn directory_sizes_are_added_replaced_and_removed() {
        let first = sizes_with(b"", 4096, 100, b"my dir");
        assert_eq!(first, b"4096 100 my%20dir\n");
        let two = sizes_with(&first, 10, 200, b"other");
        assert_eq!(two, b"4096 100 my%20dir\n10 200 other\n");
        let replaced = sizes_with(&two, 8192, 300, b"my dir");
        assert_eq!(replaced, b"10 200 other\n8192 300 my%20dir\n");
        assert_eq!(sizes_without(&replaced, b"other"), b"8192 300 my%20dir\n");
        assert_eq!(sizes_without(&replaced, b"absent"), replaced);
    }

    #[test]
    fn lines_this_code_cannot_read_are_kept() {
        let existing = b"garbage\n1 2 keep\nnot a line either";
        let out = sizes_with(existing, 5, 6, b"new");
        assert_eq!(out, b"garbage\n1 2 keep\nnot a line either\n5 6 new\n");
        assert_eq!(
            sizes_without(existing, b"keep"),
            b"garbage\nnot a line either\n"
        );
    }

    #[test]
    fn a_name_that_is_a_prefix_of_another_is_not_confused() {
        let existing = b"1 1 ab\n2 2 abc\n";
        assert_eq!(sizes_without(existing, b"ab"), b"2 2 abc\n");
    }

    #[test]
    fn candidate_names_count_up() {
        assert_eq!(candidate_name(b"f.txt", 1), b"f.txt");
        assert_eq!(candidate_name(b"f.txt", 2), b"f.txt.2");
        assert_eq!(candidate_name(b"f.txt", 10), b"f.txt.10");
    }
}
