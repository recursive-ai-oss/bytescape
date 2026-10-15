//! The kernel's list of mounts (`/proc/self/mountinfo`), enough to decide
//! whether a volume can be read by the raw ext4 reader and to find its device.
//!
//! The format is documented in proc(5): fields separated by spaces, an
//! optional-fields list ended by a lone `-`, and the characters space, tab,
//! newline and backslash written as octal escapes in the path fields.

/// One mount.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mount {
    pub id: u64,
    /// The directory of the filesystem that this mount shows.
    pub root: Vec<u8>,
    pub mount_point: Vec<u8>,
    pub fs_type: String,
    pub source: Vec<u8>,
}

/// The mounts in `text`. A line that does not parse is skipped.
pub fn parse(text: &[u8]) -> Vec<Mount> {
    text.split(|&b| b == b'\n').filter_map(parse_line).collect()
}

fn parse_line(line: &[u8]) -> Option<Mount> {
    let mut fields = line.split(|&b| b == b' ').filter(|f| !f.is_empty());
    let id: u64 = std::str::from_utf8(fields.next()?).ok()?.parse().ok()?;
    let _parent = fields.next()?;
    let _device = fields.next()?;
    let root = unescape(fields.next()?);
    let mount_point = unescape(fields.next()?);
    let _options = fields.next()?;
    // Optional fields run to a lone "-".
    for field in fields.by_ref() {
        if field == b"-" {
            break;
        }
    }
    let fs_type = String::from_utf8(fields.next()?.to_vec()).ok()?;
    let source = unescape(fields.next()?);
    Some(Mount {
        id,
        root,
        mount_point,
        fs_type,
        source,
    })
}

/// Reads `\040`-style escapes.
fn unescape(field: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(field.len());
    let mut i = 0usize;
    while let Some(&byte) = field.get(i) {
        if byte == b'\\'
            && let Some(digits) = field.get(i.saturating_add(1)..i.saturating_add(4))
            && digits.iter().all(|d| (b'0'..=b'7').contains(d))
        {
            let value = digits.iter().fold(0u32, |acc, d| {
                acc.saturating_mul(8)
                    .saturating_add(u32::from(d.saturating_sub(b'0')))
            });
            if let Ok(value) = u8::try_from(value) {
                out.push(value);
                i = i.saturating_add(4);
                continue;
            }
        }
        out.push(byte);
        i = i.saturating_add(1);
    }
    out
}

/// The mount that shows `path`, when `path` is exactly a mount point. When
/// several mounts are stacked on it the newest, which is the one in view, wins.
pub fn at_exactly<'a>(mounts: &'a [Mount], path: &[u8]) -> Option<&'a Mount> {
    mounts
        .iter()
        .filter(|m| m.mount_point == path)
        .max_by_key(|m| m.id)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    const SAMPLE: &str = "\
22 1 8:2 / / rw,relatime shared:1 - ext4 /dev/sda2 rw,errors=remount-ro
30 22 0:25 / /sys rw,nosuid shared:7 - sysfs sysfs rw
41 22 8:3 /sub /mnt/with\\040space rw master:2 shared:4 - ext4 /dev/sda3 rw
42 22 8:3 / /mnt/with\\040space rw - ext2 /dev/sda3 rw
43 22 0:50 / /mnt/data rw - btrfs /dev/mapper/vg-data rw
garbage line
";

    #[test]
    fn mounts_are_read_with_their_escapes() {
        let mounts = parse(SAMPLE.as_bytes());
        assert_eq!(mounts.len(), 5);
        assert_eq!(mounts[0].mount_point, b"/");
        assert_eq!(mounts[0].fs_type, "ext4");
        assert_eq!(mounts[0].source, b"/dev/sda2");
        assert_eq!(mounts[2].mount_point, b"/mnt/with space");
        assert_eq!(mounts[2].root, b"/sub");
        // Optional fields before the dash are skipped, however many there are.
        assert_eq!(mounts[2].fs_type, "ext4");
        assert_eq!(mounts[3].fs_type, "ext2");
    }

    #[test]
    fn a_stacked_mount_is_found_by_its_newest_entry() {
        let mounts = parse(SAMPLE.as_bytes());
        let found = at_exactly(&mounts, b"/mnt/with space").unwrap();
        assert_eq!(found.id, 42);
        assert!(at_exactly(&mounts, b"/mnt").is_none());
        assert!(at_exactly(&mounts, b"/mnt/with space/").is_none());
    }

    #[test]
    fn escapes_that_are_not_complete_are_kept_as_written() {
        assert_eq!(unescape(b"a\\04"), b"a\\04");
        assert_eq!(unescape(b"a\\0409"), b"a 9");
        assert_eq!(unescape(b"\\\\"), b"\\\\");
        // A value over 255 is not a byte and is kept as written.
        assert_eq!(unescape(b"\\777"), b"\\777");
    }

    #[test]
    fn hostile_text_does_not_panic() {
        for text in [
            &b""[..],
            b"\n\n",
            b"1",
            b"1 2 3 4 5 6 - ",
            b"x y z",
            &[0xff; 64],
        ] {
            let _ = parse(text);
        }
    }
}
