//! The kernel's list of mounts (`/proc/self/mountinfo`), enough to decide
//! which raw reader a volume can use, to find its device, and to tell which
//! mounts belong to one filesystem.
//!
//! The format is documented in proc(5): fields separated by spaces, an
//! optional-fields list ended by a lone `-`, and the characters space, tab,
//! newline and backslash written as octal escapes in the path fields.

/// One mount.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mount {
    pub id: u64,
    /// The mount this one is attached to.
    pub parent: u64,
    /// The device number the table shows (`major:minor`). Every mount of one
    /// filesystem shows the same one; separate instances of a filesystem that
    /// has no device (tmpfs, for one) show different ones.
    pub device: (u32, u32),
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
    let parent: u64 = std::str::from_utf8(fields.next()?).ok()?.parse().ok()?;
    let device = {
        let text = std::str::from_utf8(fields.next()?).ok()?;
        let (major, minor) = text.split_once(':')?;
        (major.parse().ok()?, minor.parse().ok()?)
    };
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
        parent,
        device,
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

/// The mount in view among `candidates`, which are mounts on one path: the one
/// that no other candidate is attached to. Mount IDs are handed out lowest
/// free first, so a higher ID is not a later mount; the parent links say what
/// is stacked on what. If the links do not settle it (more than one
/// candidate is on top), the highest ID decides.
fn on_top<'a>(candidates: impl Iterator<Item = &'a Mount>) -> Option<&'a Mount> {
    let all: Vec<&Mount> = candidates.collect();
    all.iter()
        .copied()
        .filter(|m| !all.iter().any(|other| other.parent == m.id))
        .max_by_key(|m| m.id)
        .or_else(|| all.iter().copied().max_by_key(|m| m.id))
}

/// The mount that shows `path`, when `path` is exactly a mount point. When
/// several mounts are stacked on it the one in view wins.
pub fn at_exactly<'a>(mounts: &'a [Mount], path: &[u8]) -> Option<&'a Mount> {
    on_top(mounts.iter().filter(|m| m.mount_point == path))
}

/// Whether `path` is `folder` or inside it, by whole path parts.
fn is_under(path: &[u8], folder: &[u8]) -> bool {
    if folder == b"/" {
        return path.first() == Some(&b'/');
    }
    path.starts_with(folder)
        && (path.len() == folder.len() || path.get(folder.len()) == Some(&b'/'))
}

/// The mount that holds `path`: the one with the longest mount point that
/// `path` is at or inside, and the one in view when several are stacked.
pub fn containing<'a>(mounts: &'a [Mount], path: &[u8]) -> Option<&'a Mount> {
    let longest = mounts
        .iter()
        .filter(|m| is_under(path, &m.mount_point))
        .map(|m| m.mount_point.len())
        .max()?;
    on_top(
        mounts
            .iter()
            .filter(|m| m.mount_point.len() == longest && is_under(path, &m.mount_point)),
    )
}

/// Whether two mounts show the same filesystem: the same device number and
/// the same type. Subvolume mounts and bind mounts of one filesystem do; a
/// second instance of tmpfs, or a different disk, does not.
pub fn same_filesystem(a: &Mount, b: &Mount) -> bool {
    a.device == b.device && a.fs_type == b.fs_type
}

fn depth(path: &[u8]) -> usize {
    path.split(|&b| b == b'/').filter(|s| !s.is_empty()).count()
}

/// Whether `mount` is attached inside the tree that `other` shows, on the
/// folder where `other` shows `mount`'s own folder (or above it). Then
/// `other` cannot be what shows that folder: `mount` is in the way. The top
/// level of a btrfs filesystem mounted at `/` shows the subvolume `home` as
/// the folder `/home`, and the mount of that subvolume at `/home` hides that
/// folder, so its files are reachable only through the second mount.
fn hides_its_own_folder(mount: &Mount, other: &Mount) -> bool {
    if mount.mount_point == other.mount_point || !is_under(&mount.mount_point, &other.mount_point) {
        return false;
    }
    // Where `other` shows the folder `mount.root`.
    let relative = if other.root == b"/" {
        mount.root.as_slice()
    } else {
        mount.root.get(other.root.len()..).unwrap_or_default()
    };
    let mut shown_at = if other.mount_point == b"/" {
        Vec::new()
    } else {
        other.mount_point.clone()
    };
    shown_at.extend_from_slice(relative);
    if shown_at.is_empty() {
        shown_at.push(b'/');
    }
    is_under(&shown_at, &mount.mount_point)
}

/// Which of the `requested` mounts a scan that starts at `root`, on the mount
/// `home`, must not enter because it would count files twice:
///
/// - a mount whose root contains `home`'s root shows the start of the scan
///   again (the top of a btrfs filesystem mounted elsewhere, while the scan
///   starts in one of its subvolumes);
/// - a mount whose root lies inside the root of another mount of the same
///   filesystem that the scan reaches (`home` itself, or another requested
///   mount below `root`), unless it is mounted in that mount's way, on the
///   very folder where that mount would show it. A bind mount of a folder the
///   scan already reaches is the usual case.
///
/// It is judged from the scan's start, using the helper's own table. Mounts
/// the scan never reaches (above `root`, or not requested) do not cover
/// anything: a drive-wide view would drop a mount the scan has to enter when
/// the scan starts somewhere other than the drive's main mount. Mounts hidden
/// under another mount are not in view, so they neither cover nor are covered.
pub fn already_shown_ids(
    mounts: &[Mount],
    home: &Mount,
    root: &[u8],
    requested: &[u64],
) -> Vec<u64> {
    let wanted: std::collections::HashSet<u64> = requested.iter().copied().collect();
    let shows_the_start = |m: &Mount| m.id != home.id && is_under(&home.root, &m.root);
    let mut dropped: Vec<u64> = mounts
        .iter()
        .filter(|m| wanted.contains(&m.id) && same_filesystem(m, home) && shows_the_start(m))
        .map(|m| m.id)
        .collect();
    let mut family: Vec<(usize, &Mount)> = mounts
        .iter()
        .enumerate()
        .filter(|(_, m)| {
            let reached = m.id == home.id
                || (wanted.contains(&m.id)
                    && is_under(&m.mount_point, root)
                    && !shows_the_start(m));
            reached
                && same_filesystem(m, home)
                && at_exactly(mounts, &m.mount_point).is_some_and(|top| top.id == m.id)
        })
        .collect();
    // The start first, then the mount showing the top of the filesystem, then
    // the nearest to `/`, then the shortest name, then the earliest in the
    // table.
    family.sort_by_key(|(position, m)| {
        (
            m.id != home.id,
            m.root != b"/",
            depth(&m.mount_point),
            m.mount_point.len(),
            *position,
        )
    });
    dropped.extend(
        family
            .iter()
            .enumerate()
            .filter(|(index, (_, member))| {
                member.id != home.id
                    && family.iter().enumerate().any(|(other_index, (_, other))| {
                        other_index != *index
                            && is_under(&member.root, &other.root)
                            && (member.root != other.root || other_index < *index)
                            && !hides_its_own_folder(member, other)
                    })
            })
            .map(|(_, (_, member))| member.id),
    );
    dropped
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

    #[test]
    fn parents_and_devices_are_read() {
        let mounts = parse(SAMPLE.as_bytes());
        assert_eq!(
            (mounts[0].id, mounts[0].parent, mounts[0].device),
            (22, 1, (8, 2))
        );
        assert_eq!((mounts[4].parent, mounts[4].device), (22, (0, 50)));
        // A line whose parent or device is not a number is skipped.
        let text = b"1 x 8:2 / / rw - ext4 /dev/sda rw\n2 1 8-2 / /a rw - ext4 /dev/sda rw\n3 1 8:2 / /b rw - ext4 /dev/sda rw\n";
        assert_eq!(parse(text).len(), 1);
    }

    #[test]
    fn the_mount_in_view_is_the_one_nothing_is_stacked_on_whatever_the_ids() {
        // Ids are handed out lowest free first, so the mount on top can have
        // the lower id: 7 is mounted on /x, and 3 (reusing a freed id) is
        // mounted over it.
        let text = b"7 1 8:2 / /x rw - ext4 /dev/sda2 rw\n3 7 0:50 / /x rw - tmpfs tmpfs rw\n";
        let mounts = parse(text);
        assert_eq!(at_exactly(&mounts, b"/x").unwrap().id, 3);
        assert_eq!(containing(&mounts, b"/x/deep/er").unwrap().id, 3);
        // Without links that settle it, the highest id is taken.
        let text = b"4 1 8:2 / /y rw - ext4 /dev/sda2 rw\n9 1 8:2 / /y rw - ext4 /dev/sda2 rw\n";
        assert_eq!(at_exactly(&parse(text), b"/y").unwrap().id, 9);
    }

    #[test]
    fn the_holding_mount_is_the_longest_one_by_whole_parts() {
        let text = b"1 0 8:2 / / rw - ext4 /dev/sda2 rw\n2 1 8:3 / /mnt rw - ext4 /dev/sda3 rw\n3 2 8:4 / /mnt/data rw - xfs /dev/sda4 rw\n";
        let mounts = parse(text);
        for (path, want) in [
            (&b"/"[..], 1),
            (b"/etc/passwd", 1),
            (b"/mnt", 2),
            (b"/mnt/other", 2),
            (b"/mnt/data", 3),
            (b"/mnt/data/x/y", 3),
            // A name that merely starts like a mount point is not inside it.
            (b"/mnt/database", 2),
            (b"/mntx", 1),
        ] {
            assert_eq!(containing(&mounts, path).unwrap().id, want, "{path:?}");
        }
        assert!(containing(&mounts, b"relative").is_none());
    }

    /// The requested mounts a scan of `root` on mount `home` must not enter.
    fn dropped(text: &str, home: u64, root: &str, requested: &[u64]) -> Vec<u64> {
        let mounts = parse(text.as_bytes());
        let home = mounts.iter().find(|m| m.id == home).unwrap();
        let mut ids = already_shown_ids(&mounts, home, root.as_bytes(), requested);
        ids.sort_unstable();
        ids
    }

    #[test]
    fn mounts_that_show_what_the_scan_reaches_anyway_are_dropped() {
        // A repeat of the top level, and mounts of folders the top level
        // shows, are the same files again.
        let text = "\
1 0 0:27 / /data rw - btrfs /dev/sdb1 rw
3 1 0:27 /@a /data/a rw - btrfs /dev/sdb1 rw
4 1 0:27 /@b /data/b rw - btrfs /dev/sdb1 rw
5 1 0:27 /@a /data/srv/a rw - btrfs /dev/sdb1 rw
6 1 0:27 / /data/again rw - btrfs /dev/sdb1 rw
";
        assert_eq!(dropped(text, 1, "/data", &[3, 4, 5, 6]), vec![3, 4, 5, 6]);
        // Mounted on the folders where the top level shows them, subvolumes are
        // in its way and not dropped; a second mount of one is.
        let text = "\
1 0 0:27 / / rw - btrfs /dev/sda3 rw
2 1 0:27 /home /home rw - btrfs /dev/sda3 rw
3 1 0:27 /var/log /var/log rw - btrfs /dev/sda3 rw
4 1 0:27 /home /mnt/home-again rw - btrfs /dev/sda3 rw
";
        assert_eq!(dropped(text, 1, "/", &[2, 3, 4]), vec![4]);
        // Nothing is dropped that was not asked for.
        assert!(dropped(text, 1, "/", &[]).is_empty());
    }

    #[test]
    fn the_scans_own_start_decides_not_the_drives_main_mount() {
        // `/` is the subvolume @, @home is mounted at /home, and the top level
        // is mounted elsewhere. The top level is not part of a scan of `/`,
        // so it does not cover /home; but it shows the start again.
        let text = "\
1 0 0:27 /@ / rw - btrfs /dev/sda3 rw
2 1 0:27 /@home /home rw - btrfs /dev/sda3 rw
3 1 0:27 / /mnt/top rw - btrfs /dev/sda3 rw
";
        assert!(dropped(text, 1, "/", &[2]).is_empty());
        assert_eq!(dropped(text, 1, "/", &[3]), vec![3]);
        assert_eq!(dropped(text, 1, "/", &[2, 3]), vec![3]);
        // A mount elsewhere that the scan never reaches does not cover what the
        // scan does reach: /mnt/a shows @a, which holds @a/b, but a scan of
        // /mnt/c has to enter the mount of @a/b below it.
        let text = "\
1 0 0:27 /@c /mnt/c rw - btrfs /dev/sda3 rw
2 1 0:27 /@a/b /mnt/c/b rw - btrfs /dev/sda3 rw
3 1 0:27 /@a /mnt/a rw - btrfs /dev/sda3 rw
";
        assert!(dropped(text, 1, "/mnt/c", &[2, 3]).is_empty());
        // Without the top-level mount nothing changes for /home.
        let text = "\
1 0 0:27 /@ / rw - btrfs /dev/sda3 rw
2 1 0:27 /@home /home rw - btrfs /dev/sda3 rw
";
        assert!(dropped(text, 1, "/", &[2]).is_empty());
    }

    #[test]
    fn what_is_not_in_the_family_or_in_view_is_left_alone() {
        // Another filesystem is never part of the family, whatever its roots.
        let text = "\
1 0 8:2 / /mnt rw - ext4 /dev/sda2 rw
2 1 8:3 / /mnt/x rw - ext4 /dev/sda3 rw
";
        assert!(dropped(text, 1, "/mnt", &[2]).is_empty());
        // A mount hidden under another is not in view: it neither covers nor
        // is covered.
        let text = "\
1 0 8:2 / /m rw - ext4 /dev/sda2 rw
2 1 8:2 /x /m/hidden rw - ext4 /dev/sda2 rw
3 1 0:50 / /m/hidden rw - tmpfs tmpfs rw
";
        assert!(dropped(text, 1, "/m", &[2]).is_empty());
        // An empty table drops nothing.
        let home = parse(b"1 0 8:1 / / rw - ext4 /dev/sda1 rw");
        assert!(already_shown_ids(&[], &home[0], b"/", &[1, 2]).is_empty());
    }

    #[test]
    fn mounts_of_one_filesystem_are_told_from_others_by_device_and_type() {
        let text = b"1 0 0:27 /@ / rw - btrfs /dev/sda3 rw\n2 1 0:27 /@home /home rw - btrfs /dev/sda3 rw\n3 1 0:28 / /other rw - btrfs /dev/sdb1 rw\n4 1 0:50 / /t1 rw - tmpfs tmpfs rw\n5 1 0:51 / /t2 rw - tmpfs tmpfs rw\n";
        let m = parse(text);
        assert!(same_filesystem(&m[0], &m[1]));
        assert!(!same_filesystem(&m[0], &m[2]));
        assert!(!same_filesystem(&m[3], &m[4]), "two tmpfs instances");
    }
}
