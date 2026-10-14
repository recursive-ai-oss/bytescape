//! Parser for `/proc/self/mountinfo`.
//!
//! The input is whatever the kernel (or a hostile test) produced. Parsing
//! never panics: a line that does not fit the format is skipped, and an
//! over-long line is ignored.

/// Longest line that is parsed. Real lines are a few hundred bytes; the cap
/// keeps a corrupt file from costing much.
const MAX_LINE: usize = 64 * 1024;

/// One mount.
///
/// Paths are raw bytes because Linux paths need not be UTF-8.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MountEntry {
    pub mount_id: u32,
    pub major: u32,
    pub minor: u32,
    /// The directory of the filesystem that this mount exposes. `/` for a
    /// whole filesystem, `/@home` for a btrfs subvolume.
    pub root: Vec<u8>,
    pub mount_point: Vec<u8>,
    /// Per-mount options, such as `ro,relatime`.
    pub options: String,
    pub fs_type: String,
    pub source: Vec<u8>,
    /// Per-filesystem options, such as `rw,discard`.
    pub super_options: String,
}

impl MountEntry {
    /// Whether the mount or its filesystem is read-only.
    pub fn read_only(&self) -> bool {
        has_option(&self.options, "ro") || has_option(&self.super_options, "ro")
    }
}

fn has_option(options: &str, wanted: &str) -> bool {
    options.split(',').any(|option| option == wanted)
}

/// Parses a whole file. Lines that do not parse are skipped.
pub(crate) fn parse(text: &[u8]) -> Vec<MountEntry> {
    text.split(|&b| b == b'\n')
        .filter(|line| line.len() <= MAX_LINE)
        .filter_map(parse_line)
        .collect()
}

/// Parses one line:
/// `id parent major:minor root mount-point options [optional...] - fstype source super-options`.
pub(crate) fn parse_line(line: &[u8]) -> Option<MountEntry> {
    let fields: Vec<&[u8]> = line.split(|&b| b == b' ').collect();
    if fields.len() < 10 {
        return None;
    }
    let mount_id = number(fields[0])?;
    let _parent_id: u32 = number(fields[1])?;
    let (major, minor) = device_numbers(fields[2])?;
    let root = unescape(fields[3]);
    let mount_point = unescape(fields[4]);
    let options = text(fields[5]);
    // Zero or more optional fields (`shared:1`, `master:2`) follow, ended by `-`.
    let separator = fields.iter().skip(6).position(|f| *f == b"-")? + 6;
    let rest = &fields[separator + 1..];
    if rest.len() < 2 {
        return None;
    }
    let fs_type = text(rest[0]);
    if fs_type.is_empty() || mount_point.is_empty() || mount_point[0] != b'/' {
        return None;
    }
    Some(MountEntry {
        mount_id,
        major,
        minor,
        root,
        mount_point,
        options,
        fs_type,
        source: unescape(rest[1]),
        super_options: rest.get(2).map(|f| text(f)).unwrap_or_default(),
    })
}

fn number(field: &[u8]) -> Option<u32> {
    std::str::from_utf8(field).ok()?.parse().ok()
}

fn device_numbers(field: &[u8]) -> Option<(u32, u32)> {
    let text = std::str::from_utf8(field).ok()?;
    let (major, minor) = text.split_once(':')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

fn text(field: &[u8]) -> String {
    String::from_utf8_lossy(&unescape(field)).into_owned()
}

/// Undoes the kernel's escaping: a backslash followed by three octal digits is
/// one byte (`\040` is a space, `\011` a tab, `\012` a newline, `\134` a
/// backslash). Any other backslash is kept as it is.
pub(crate) fn unescape(field: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(field.len());
    let mut index = 0;
    while index < field.len() {
        if field[index] == b'\\'
            && let Some(digits) = field.get(index + 1..index + 4)
            && digits.iter().all(|d| (b'0'..=b'7').contains(d))
        {
            let value = u32::from(digits[0] - b'0') * 64
                + u32::from(digits[1] - b'0') * 8
                + u32::from(digits[2] - b'0');
            // Three octal digits reach 511; only a byte is valid.
            if let Ok(byte) = u8::try_from(value) {
                out.push(byte);
                index += 4;
                continue;
            }
        }
        out.push(field[index]);
        index += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn parses_the_documented_example() {
        let line =
            b"36 35 98:0 /mnt1 /mnt2 rw,noatime master:1 - ext3 /dev/root rw,errors=continue";
        let entry = parse_line(line).unwrap();
        assert_eq!(entry.mount_id, 36);
        assert_eq!((entry.major, entry.minor), (98, 0));
        assert_eq!(entry.root, b"/mnt1");
        assert_eq!(entry.mount_point, b"/mnt2");
        assert_eq!(entry.options, "rw,noatime");
        assert_eq!(entry.fs_type, "ext3");
        assert_eq!(entry.source, b"/dev/root");
        assert_eq!(entry.super_options, "rw,errors=continue");
        assert!(!entry.read_only());
    }

    #[test]
    fn no_optional_fields_and_many_optional_fields() {
        let none = b"21 1 8:1 / /boot rw - ext4 /dev/sda1 rw";
        assert_eq!(parse_line(none).unwrap().fs_type, "ext4");
        let many = b"21 1 8:1 / /boot rw shared:1 master:2 propagate_from:3 unbindable - ext4 /dev/sda1 rw";
        assert_eq!(parse_line(many).unwrap().source, b"/dev/sda1");
    }

    #[test]
    fn escapes_become_bytes() {
        let line = br"70 1 8:5 /My\040Disk /mnt/a\040b\011c\012d\134e rw - ext4 /dev/sdb1 ro";
        let entry = parse_line(line).unwrap();
        assert_eq!(entry.root, b"/My Disk");
        assert_eq!(entry.mount_point, b"/mnt/a b\tc\nd\\e");
        assert!(entry.read_only(), "super options say ro");
    }

    #[test]
    fn unescape_leaves_odd_sequences_alone() {
        assert_eq!(unescape(br"a\04"), br"a\04");
        assert_eq!(unescape(br"a\0x1"), br"a\0x1");
        assert_eq!(unescape(br"\9"), br"\9");
        assert_eq!(unescape(br"trailing\"), br"trailing\");
        // 0o777 is above a byte; it stays as written.
        assert_eq!(unescape(br"\777"), br"\777");
        assert_eq!(unescape(br"\377"), vec![0xff]);
    }

    #[test]
    fn non_utf8_paths_survive() {
        let mut line = b"70 1 8:5 / /mnt/".to_vec();
        line.extend_from_slice(&[0xff, 0xfe]);
        line.extend_from_slice(b" rw - ext4 /dev/sdb1 rw");
        let entry = parse_line(&line).unwrap();
        assert_eq!(
            entry.mount_point,
            [b"/mnt/".as_slice(), &[0xff, 0xfe]].concat()
        );
    }

    #[test]
    fn read_only_is_a_whole_option() {
        let line = b"1 1 8:1 / /a rw,nosuid - ext4 /dev/sda1 rw,errors=remount-ro";
        assert!(!parse_line(line).unwrap().read_only());
        let line = b"1 1 8:1 / /a ro,nosuid - ext4 /dev/sda1 rw";
        assert!(parse_line(line).unwrap().read_only());
    }

    #[test]
    fn malformed_lines_are_rejected() {
        let bad: [&[u8]; 13] = [
            b"",
            b" ",
            b"garbage",
            // Too few fields.
            b"36 35 98:0 /mnt1 /mnt2 rw - ext3",
            b"36 35 98:0 / /m rw - ext3",
            // Bad numbers.
            b"x 35 98:0 /mnt1 /mnt2 rw - ext3 /dev/root rw",
            b"36 x 98:0 /mnt1 /mnt2 rw - ext3 /dev/root rw",
            b"36 35 98 /mnt1 /mnt2 rw - ext3 /dev/root rw",
            b"36 35 a:b /mnt1 /mnt2 rw - ext3 /dev/root rw",
            b"99999999999 35 98:0 / /m rw - ext3 /dev/root rw",
            // No separator before the file system type.
            b"36 35 98:0 /mnt1 /mnt2 rw ext3 /dev/root rw extra more",
            // Mount point not absolute, or empty.
            b"36 35 98:0 /mnt1 relative rw - ext3 /dev/root rw",
            b"36 35 98:0 /mnt1  rw - ext3 /dev/root rw",
        ];
        for line in bad {
            assert_eq!(
                parse_line(line),
                None,
                "{:?}",
                String::from_utf8_lossy(line)
            );
        }
    }

    #[test]
    fn overlong_lines_are_ignored_and_the_rest_still_parses() {
        let mut text = vec![b'a'; MAX_LINE + 1];
        text.push(b'\n');
        text.extend_from_slice(b"21 1 8:1 / /boot rw - ext4 /dev/sda1 rw\n");
        let entries = parse(&text);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].mount_point, b"/boot");
    }

    #[test]
    fn blank_lines_and_missing_final_newline() {
        let text =
            b"\n\n21 1 8:1 / /a rw - ext4 /dev/sda1 rw\n\n22 1 8:2 / /b rw - ext4 /dev/sda2 rw";
        let entries = parse(text);
        assert_eq!(entries.len(), 2);
    }

    proptest! {
        #[test]
        fn random_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
            let _ = parse(&bytes);
        }

        #[test]
        fn plausible_lines_with_hostile_fields_never_panic(
            a in proptest::collection::vec(any::<u8>(), 0..40),
            b in proptest::collection::vec(any::<u8>(), 0..40),
            c in proptest::collection::vec(any::<u8>(), 0..40),
        ) {
            let mut line = b"1 1 8:1 ".to_vec();
            line.extend_from_slice(&a);
            line.push(b' ');
            line.extend_from_slice(&b);
            line.extend_from_slice(b" rw - ");
            line.extend_from_slice(&c);
            line.extend_from_slice(b" /dev/x rw");
            if let Some(entry) = parse_line(&line) {
                prop_assert!(entry.mount_point.first() == Some(&b'/'));
            }
        }

        #[test]
        fn unescape_never_grows(bytes in proptest::collection::vec(any::<u8>(), 0..200)) {
            prop_assert!(unescape(&bytes).len() <= bytes.len());
        }
    }
}
