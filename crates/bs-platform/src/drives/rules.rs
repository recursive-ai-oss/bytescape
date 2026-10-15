//! Which mounts are drives, what kind each is, and how mounts merge.
//!
//! This is pure logic over parsed mount entries so the rules can be tested on
//! recorded mount tables without touching the machine.

use super::DriveKind;
use super::mountinfo::MountEntry;
use std::collections::HashMap;

/// What the rules need to ask the machine.
pub(crate) trait Devices {
    /// Whether the device behind this mount is removable: attached over USB,
    /// flagged removable by the kernel, or a memory card.
    fn is_removable(&self, entry: &MountEntry) -> bool;
}

/// Filesystems on disks. A mount of one of these is a drive.
const DISK_FS: [&str; 21] = [
    "ext2", "ext3", "ext4", "btrfs", "xfs", "f2fs", "jfs", "reiserfs", "nilfs2", "bcachefs", "zfs",
    "vfat", "msdos", "exfat", "ntfs", "ntfs3", "fuseblk", "hfs", "hfsplus", "udf", "iso9660",
];

/// Filesystems on optical discs and disc images.
const OPTICAL_FS: [&str; 2] = ["udf", "iso9660"];

/// Filesystems that are served over a network.
const NETWORK_FS: [&str; 19] = [
    "nfs",
    "nfs4",
    "cifs",
    "smb3",
    "smbfs",
    "ncpfs",
    "afs",
    "9p",
    "ceph",
    "glusterfs",
    "davfs",
    "fuse.glusterfs",
    "fuse.sshfs",
    "sshfs",
    "fuse.davfs",
    "fuse.rclone",
    "fuse.s3fs",
    "fuse.gcsfuse",
    "fuse.curlftpfs",
];

/// User-space filesystems that exist for the desktop and are not storage.
const HIDDEN_FUSE: [&str; 5] = [
    "fuse.portal",
    "fuse.gvfsd-fuse",
    "fuse.lxcfs",
    "fuse.snapfuse",
    "fuse.squashfuse",
];

/// Mount points that are never listed: the boot partitions the firmware
/// reads, and the mounts the Windows Subsystem for Linux makes for itself.
const HIDDEN_MOUNT_POINTS: [&[u8]; 3] = [b"/boot/efi", b"/efi", b"/boot/firmware"];
const HIDDEN_MOUNT_PREFIXES: [&[u8]; 3] = [b"/run/user/", b"/usr/lib/wsl/", b"/mnt/wslg/"];

/// One filesystem, possibly mounted several times.
#[derive(Clone, Debug)]
pub(crate) struct Candidate {
    /// The mount that stands for the drive.
    pub entry: MountEntry,
    /// Every visible mount of the same filesystem, the representative included.
    pub members: Vec<MountEntry>,
    pub kind: DriveKind,
    /// Not shown in the list of drives, though the sizes of a folder on it can
    /// still be asked for.
    pub hidden: bool,
}

/// Decides the kind of a mount, or `None` when it is not a drive.
pub(crate) fn classify(entry: &MountEntry, devices: &dyn Devices) -> Option<DriveKind> {
    let fs = entry.fs_type.as_str();
    let mount_point = entry.mount_point.as_slice();
    if HIDDEN_MOUNT_POINTS.contains(&mount_point)
        || HIDDEN_MOUNT_PREFIXES
            .iter()
            .any(|prefix| mount_point.starts_with(prefix))
    {
        return None;
    }
    if NETWORK_FS.contains(&fs) {
        return Some(DriveKind::Network);
    }
    if DISK_FS.contains(&fs) {
        // An attached disk image, such as a loop-mounted installer.
        if entry.source.starts_with(b"/dev/loop") {
            return Some(DriveKind::Other);
        }
        if OPTICAL_FS.contains(&fs) || devices.is_removable(entry) {
            return Some(DriveKind::Removable);
        }
        return Some(DriveKind::Fixed);
    }
    if fs == "fuse" || fs.starts_with("fuse.") {
        if HIDDEN_FUSE.contains(&fs) {
            return None;
        }
        return Some(DriveKind::Other);
    }
    // Everything else (proc, tmpfs, overlay, squashfs, cgroups, nsfs, and any
    // type not known to hold user files) is not a drive.
    None
}

/// The identity of the filesystem behind a mount. Two mounts with the same
/// key are the same drive.
fn group_key(entry: &MountEntry) -> Vec<u8> {
    let mut key = Vec::new();
    let fs = entry.fs_type.as_str();
    if NETWORK_FS.contains(&fs) {
        key.extend_from_slice(b"net\0");
        key.extend_from_slice(fs.as_bytes());
        key.push(0);
        key.extend_from_slice(&entry.source);
    } else if entry.source.starts_with(b"/dev/") {
        // The device node, not its number: the subvolumes of one btrfs
        // filesystem each get a different number but share the node.
        key.extend_from_slice(b"dev\0");
        key.extend_from_slice(&entry.source);
    } else if fs == "zfs" {
        key.extend_from_slice(b"zfs\0");
        let pool = entry.source.split(|&b| b == b'/').next().unwrap_or(&[]);
        key.extend_from_slice(pool);
    } else {
        key.extend_from_slice(format!("mount\0{}:{}\0{fs}\0", entry.major, entry.minor).as_bytes());
        key.extend_from_slice(&entry.source);
    }
    key
}

fn depth(path: &[u8]) -> usize {
    path.split(|&b| b == b'/').filter(|s| !s.is_empty()).count()
}

/// Orders the mounts of one filesystem: the one exposing the top of the
/// filesystem first, then the nearest to `/`, then the shortest name, then the
/// earliest mounted.
fn representative_rank(position: usize, entry: &MountEntry) -> (bool, usize, usize, usize) {
    (
        entry.root != b"/",
        depth(&entry.mount_point),
        entry.mount_point.len(),
        position,
    )
}

/// The mounts of one filesystem, each with its position in the mount table,
/// under the key that identifies the filesystem.
type Group = (Vec<u8>, Vec<(usize, MountEntry)>);

/// Turns a mount table into candidates, one per filesystem.
///
/// When several mounts cover the same directory only the last one is visible,
/// so the earlier ones are dropped first.
pub(crate) fn plan(entries: Vec<MountEntry>, devices: &dyn Devices) -> Vec<Candidate> {
    let mut last_at: HashMap<&[u8], usize> = HashMap::new();
    for (position, entry) in entries.iter().enumerate() {
        last_at.insert(entry.mount_point.as_slice(), position);
    }
    let visible: Vec<(usize, MountEntry)> = entries
        .iter()
        .enumerate()
        .filter(|(position, entry)| last_at.get(entry.mount_point.as_slice()) == Some(position))
        .map(|(position, entry)| (position, entry.clone()))
        .collect();

    let mut groups: Vec<Group> = Vec::new();
    let mut index_of: HashMap<Vec<u8>, usize> = HashMap::new();
    for (position, entry) in visible {
        let key = group_key(&entry);
        match index_of.get(&key) {
            Some(&index) => groups[index].1.push((position, entry)),
            None => {
                index_of.insert(key.clone(), groups.len());
                groups.push((key, vec![(position, entry)]));
            }
        }
    }

    groups
        .into_iter()
        .map(|(_, mut members)| {
            members.sort_by_key(|(position, entry)| representative_rank(*position, entry));
            let entry = members[0].1.clone();
            let classified = classify(&entry, devices);
            Candidate {
                kind: classified.unwrap_or(DriveKind::Other),
                hidden: classified.is_none(),
                members: members.into_iter().map(|(_, e)| e).collect(),
                entry,
            }
        })
        .collect()
}

/// Whether `path` is `mount_point` or below it, by whole components.
pub(crate) fn is_under(path: &[u8], mount_point: &[u8]) -> bool {
    if mount_point == b"/" {
        return path.first() == Some(&b'/');
    }
    path.starts_with(mount_point)
        && (path.len() == mount_point.len() || path[mount_point.len()] == b'/')
}

/// For each mount of one filesystem (in representative order), whether another
/// mount already shows its folder: that mount's root contains this one's, or
/// is the same and comes earlier in the list.
///
/// A mount that is attached on the very folder the other mount would show it
/// in is not covered: the top level of a btrfs filesystem mounted at `/` shows
/// the subvolume `home` as the folder `/home`, and the mount of that subvolume
/// at `/home` hides that folder. The files are reachable only through the
/// second mount, so a scan has to enter it.
pub(crate) fn covered_flags(members: &[MountEntry]) -> Vec<bool> {
    // Only a mount that is entered shows anything, so a mount that is covered
    // itself covers nothing. Without that, a mount in the way of one covering
    // mount could be flagged for a second covering mount that is a repeat of
    // the first: its files would be reached through neither. What shows a
    // folder has a root above it or equal to it, so the mounts are judged from
    // the shortest root down, and every coverer is decided before the mounts
    // it could cover.
    let depth = |root: &[u8]| root.split(|&b| b == b'/').filter(|s| !s.is_empty()).count();
    let mut order: Vec<usize> = (0..members.len()).collect();
    order.sort_by_key(|&index| (members.get(index).map_or(0, |m| depth(&m.root)), index));
    let mut covered = vec![false; members.len()];
    for index in order {
        let Some(member) = members.get(index) else {
            continue;
        };
        let by_another = members.iter().enumerate().any(|(other_index, other)| {
            other_index != index
                && !covered.get(other_index).copied().unwrap_or(false)
                && is_under(&member.root, &other.root)
                && (member.root != other.root || other_index < index)
                && !hides_its_own_folder(member, other)
        });
        if let Some(slot) = covered.get_mut(index) {
            *slot = by_another;
        }
    }
    covered
}

/// Whether `mount` is attached inside the tree that `other` shows, on the
/// folder where `other` shows `mount`'s own folder (or above it). Then
/// `other` cannot be what shows that folder: `mount` is in the way.
fn hides_its_own_folder(mount: &MountEntry, other: &MountEntry) -> bool {
    // `mount` has to be attached somewhere inside `other`'s tree.
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

/// Finds the candidate that holds a folder.
///
/// `device` is the folder's own device number and `path` its canonical path.
/// A mount whose device number matches and whose mount point is the longest
/// prefix of the path wins. When no mount has the device number (for example,
/// the mount table belongs to another mount namespace), the longest prefix
/// alone decides.
pub(crate) fn find<'a>(
    candidates: &'a [Candidate],
    device: (u32, u32),
    path: &[u8],
) -> Option<&'a Candidate> {
    let mut best_with_device: Option<(&Candidate, usize)> = None;
    let mut first_with_device: Option<&Candidate> = None;
    let mut best_any: Option<(&Candidate, usize)> = None;
    for candidate in candidates {
        for member in &candidate.members {
            let prefix = is_under(path, &member.mount_point);
            let length = member.mount_point.len();
            if prefix && best_any.is_none_or(|(_, l)| length > l) {
                best_any = Some((candidate, length));
            }
            if (member.major, member.minor) == device {
                first_with_device.get_or_insert(candidate);
                if prefix && best_with_device.is_none_or(|(_, l)| length > l) {
                    best_with_device = Some((candidate, length));
                }
            }
        }
    }
    best_with_device
        .map(|(c, _)| c)
        .or(first_with_device)
        .or(best_any.map(|(c, _)| c))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drives::mountinfo::parse;
    use std::collections::HashSet;

    /// Marks sources as removable by name.
    struct Fake(HashSet<&'static str>);

    impl Devices for Fake {
        fn is_removable(&self, entry: &MountEntry) -> bool {
            self.0.iter().any(|s| s.as_bytes() == entry.source)
        }
    }

    fn removable(sources: &[&'static str]) -> Fake {
        Fake(sources.iter().copied().collect())
    }

    /// `(mount point, kind)` of every listed drive, in table order.
    fn listed(text: &str, devices: &Fake) -> Vec<(String, DriveKind)> {
        plan(parse(text.as_bytes()), devices)
            .into_iter()
            .filter(|c| !c.hidden)
            .map(|c| {
                (
                    String::from_utf8_lossy(&c.entry.mount_point).into_owned(),
                    c.kind,
                )
            })
            .collect()
    }

    fn row(mount: &str, kind: DriveKind) -> (String, DriveKind) {
        (mount.to_owned(), kind)
    }

    #[test]
    fn ubuntu_desktop_with_snaps() {
        let text = include_str!("../../tests/fixtures/mountinfo/ubuntu-desktop.txt");
        let drives = listed(text, &removable(&["/dev/sdb1"]));
        assert_eq!(
            drives,
            vec![
                row("/", DriveKind::Fixed),
                row("/data", DriveKind::Fixed),
                row("/home", DriveKind::Fixed),
                row("/media/alice/USB STICK", DriveKind::Removable),
            ],
            "snap loops, tmpfs, cgroups, nsfs, overlay, the portal, the EFI partition and \
             the bind mount of /home must not appear"
        );
    }

    #[test]
    fn bind_mount_merges_into_the_real_mount() {
        let text = include_str!("../../tests/fixtures/mountinfo/ubuntu-desktop.txt");
        let plan = plan(parse(text.as_bytes()), &removable(&[]));
        let home = plan
            .iter()
            .find(|c| c.entry.mount_point == b"/home")
            .unwrap();
        assert_eq!(home.members.len(), 2, "/home and /srv/projects");
        assert_eq!(
            home.entry.root, b"/",
            "the whole-filesystem mount represents it"
        );
    }

    #[test]
    fn btrfs_subvolumes_are_one_drive() {
        let text = include_str!("../../tests/fixtures/mountinfo/btrfs-subvolumes.txt");
        let plan = plan(parse(text.as_bytes()), &removable(&[]));
        let btrfs: Vec<_> = plan.iter().filter(|c| c.entry.fs_type == "btrfs").collect();
        assert_eq!(btrfs.len(), 1);
        // Five mounts: /, /home, /var/log, /.snapshots, and the top level at /data.
        assert_eq!(btrfs[0].members.len(), 5);
        // The top-level mount (root "/") represents it even though it is deeper.
        assert_eq!(btrfs[0].entry.mount_point, b"/data");
        let drives = listed(text, &removable(&[]));
        assert_eq!(
            drives,
            vec![
                row("/data", DriveKind::Fixed),
                row("/boot", DriveKind::Fixed)
            ]
        );
    }

    #[test]
    fn btrfs_without_a_top_level_mount_is_represented_by_the_nearest_to_root() {
        let text = include_str!("../../tests/fixtures/mountinfo/fedora.txt");
        let plan = plan(parse(text.as_bytes()), &removable(&[]));
        let luks = plan
            .iter()
            .find(|c| c.entry.fs_type == "btrfs")
            .expect("btrfs");
        assert_eq!(luks.members.len(), 2);
        assert_eq!(luks.entry.mount_point, b"/");
    }

    #[test]
    fn fedora_with_luks_usb_and_network_shares() {
        let text = include_str!("../../tests/fixtures/mountinfo/fedora.txt");
        let drives = listed(text, &removable(&["/dev/sdb2"]));
        assert_eq!(
            drives,
            vec![
                row("/", DriveKind::Fixed),
                row("/boot", DriveKind::Fixed),
                row("/run/media/bob/Backup", DriveKind::Removable),
                row("/mnt/nas", DriveKind::Network),
                row("/mnt/share", DriveKind::Network),
            ],
            "gvfs and portal fuse mounts, selinuxfs, overlay and the EFI partition are hidden"
        );
    }

    #[test]
    fn wsl_like_environment() {
        let text = include_str!("../../tests/fixtures/mountinfo/wsl.txt");
        let drives = listed(text, &removable(&[]));
        assert_eq!(
            drives,
            vec![
                row("/", DriveKind::Fixed),
                row("/mnt/c", DriveKind::Network),
                row("/mnt/d", DriveKind::Network),
            ],
            "the system mounts under /usr/lib/wsl and /mnt/wslg are hidden"
        );
        let entries = parse(text.as_bytes());
        let c = entries.iter().find(|e| e.mount_point == b"/mnt/c").unwrap();
        assert_eq!(c.source, br"C:\");
    }

    #[test]
    fn escaped_mount_points() {
        let text = include_str!("../../tests/fixtures/mountinfo/escapes.txt");
        let drives = listed(text, &removable(&["/dev/sdb1"]));
        assert_eq!(
            drives,
            vec![
                row("/", DriveKind::Fixed),
                row("/mnt/My Passport", DriveKind::Removable),
                row("/mnt/tab\there", DriveKind::Fixed),
                row("/mnt/new\nline", DriveKind::Fixed),
                row("/mnt/back\\slash", DriveKind::Fixed),
                row("/mnt/Müsik", DriveKind::Fixed),
            ]
        );
    }

    fn one(line: &str, devices: &Fake) -> Option<DriveKind> {
        classify(&parse(line.as_bytes())[0], devices)
    }

    #[test]
    fn classification_table() {
        let none = removable(&[]);
        let usb = removable(&["/dev/sdb1"]);
        #[rustfmt::skip]
        let cases: [(&str, &Fake, Option<DriveKind>); 24] = [
            ("1 1 8:1 / / rw - ext4 /dev/sda1 rw", &none, Some(DriveKind::Fixed)),
            ("1 1 8:1 / /x rw - xfs /dev/sda1 rw", &none, Some(DriveKind::Fixed)),
            ("1 1 8:1 / /x rw - zfs tank/data rw", &none, Some(DriveKind::Fixed)),
            ("1 1 8:1 / /x rw - fuseblk /dev/sda1 rw", &none, Some(DriveKind::Fixed)),
            ("1 1 8:17 / /m rw - vfat /dev/sdb1 rw", &usb, Some(DriveKind::Removable)),
            ("1 1 8:17 / /m rw - exfat /dev/sdb1 rw", &usb, Some(DriveKind::Removable)),
            ("1 1 11:0 / /cd ro - iso9660 /dev/sr0 ro", &none, Some(DriveKind::Removable)),
            ("1 1 11:0 / /cd ro - udf /dev/sr0 ro", &none, Some(DriveKind::Removable)),
            ("1 1 7:3 / /img rw - ext4 /dev/loop3 rw", &none, Some(DriveKind::Other)),
            ("1 1 0:5 / /n rw - nfs4 s:/e rw", &none, Some(DriveKind::Network)),
            ("1 1 0:5 / /n rw - cifs //s/x rw", &none, Some(DriveKind::Network)),
            ("1 1 0:5 / /n rw - smb3 //s/x rw", &none, Some(DriveKind::Network)),
            ("1 1 0:5 / /n rw - fuse.sshfs u@h:/ rw", &none, Some(DriveKind::Network)),
            ("1 1 0:5 / /n rw - 9p share rw", &none, Some(DriveKind::Network)),
            ("1 1 0:5 / /f rw - fuse.cryptomator x rw", &none, Some(DriveKind::Other)),
            ("1 1 0:5 / /f rw - fuse.portal portal rw", &none, None),
            ("1 1 0:5 / /t rw - tmpfs tmpfs rw", &none, None),
            ("1 1 0:5 / /t rw - overlay overlay rw", &none, None),
            ("1 1 7:0 / /snap/x/1 ro - squashfs /dev/loop0 ro", &none, None),
            ("1 1 0:5 / /t rw - somefs-we-never-heard-of x rw", &none, None),
            ("1 1 8:2 / /boot/efi rw - vfat /dev/sda2 rw", &none, None),
            ("1 1 8:2 / /efi rw - vfat /dev/sda2 rw", &none, None),
            ("1 1 0:5 / /run/user/1000/gvfs rw - fuse.cryptomator x rw", &none, None),
            ("1 1 8:2 / /boot rw - ext4 /dev/sda2 rw", &none, Some(DriveKind::Fixed)),
        ];
        for (line, devices, expected) in cases {
            assert_eq!(one(line, devices), expected, "{line}");
        }
    }

    #[test]
    fn network_names_match_exactly_not_by_prefix() {
        let none = removable(&[]);
        assert_eq!(one("1 1 0:5 / /n rw - nfsx s rw", &none), None);
        assert_eq!(
            one("1 1 0:5 / /n rw - fuse.sshfs-wrapper s rw", &none),
            Some(DriveKind::Other)
        );
    }

    #[test]
    fn later_mount_on_the_same_directory_hides_the_earlier_one() {
        let text = "\
1 0 8:1 / / rw - ext4 /dev/sda1 rw
2 1 8:2 / /data rw - ext4 /dev/sda2 rw
3 1 8:3 / /data rw - ext4 /dev/sda3 rw
";
        let candidates = plan(parse(text.as_bytes()), &removable(&[]));
        let sources: Vec<_> = candidates.iter().map(|c| c.entry.source.clone()).collect();
        assert_eq!(sources, vec![b"/dev/sda1".to_vec(), b"/dev/sda3".to_vec()]);
    }

    #[test]
    fn network_mounts_merge_by_export_not_by_server() {
        let text = "\
1 0 0:40 / /mnt/a rw - nfs4 nas:/export/a rw
2 0 0:41 /sub /mnt/a-sub rw - nfs4 nas:/export/a rw
3 0 0:42 / /mnt/b rw - nfs4 nas:/export/b rw
";
        let candidates = plan(parse(text.as_bytes()), &removable(&[]));
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].members.len(), 2);
    }

    #[test]
    fn zfs_datasets_of_one_pool_are_one_drive() {
        let text = "\
1 0 0:40 / / rw - zfs rpool/ROOT/ubuntu rw
2 0 0:41 / /home rw - zfs rpool/USERDATA/home rw
3 0 0:42 / /tank rw - zfs tank rw
";
        let candidates = plan(parse(text.as_bytes()), &removable(&[]));
        assert_eq!(candidates.len(), 2);
    }

    #[test]
    fn mounts_without_a_device_node_are_not_merged_across_instances() {
        let text = "\
1 0 0:40 / /a rw - fuse.cryptomator vault rw
2 0 0:41 / /b rw - fuse.cryptomator vault rw
";
        assert_eq!(plan(parse(text.as_bytes()), &removable(&[])).len(), 2);
    }

    #[test]
    fn bind_mounts_are_flagged_as_covered_and_subvolumes_are_not() {
        let text = include_str!("../../tests/fixtures/mountinfo/ubuntu-desktop.txt");
        let candidates = plan(parse(text.as_bytes()), &removable(&[]));
        let home = candidates
            .iter()
            .find(|c| c.entry.mount_point == b"/home")
            .unwrap();
        let flags = covered_flags(&home.members);
        let by_point: Vec<_> = home
            .members
            .iter()
            .zip(&flags)
            .map(|(m, f)| (String::from_utf8_lossy(&m.mount_point).into_owned(), *f))
            .collect();
        assert_eq!(
            by_point,
            vec![
                ("/home".to_owned(), false),
                ("/srv/projects".to_owned(), true)
            ]
        );

        // Sibling btrfs subvolumes show different folders: none is covered.
        let text = include_str!("../../tests/fixtures/mountinfo/fedora.txt");
        let candidates = plan(parse(text.as_bytes()), &removable(&[]));
        let btrfs = candidates
            .iter()
            .find(|c| c.entry.fs_type == "btrfs")
            .unwrap();
        assert_eq!(covered_flags(&btrfs.members), vec![false, false]);
    }

    #[test]
    fn a_mount_in_the_way_is_not_covered_for_a_covering_mount_that_is_covered_itself() {
        // `/pool` shows the top of the filesystem and `/pool/home` is mounted
        // on the folder where it shows `home`: in its way, so its files are
        // reachable only through it. `/pool2` is a second mount of the top; it
        // is a repeat of `/pool` and is not entered, so it cannot be what
        // shows `home` for `/pool/home`.
        let text = "\
1 0 0:27 / /pool rw - btrfs /dev/sdd1 rw
2 1 0:27 /home /pool/home rw - btrfs /dev/sdd1 rw
3 1 0:27 / /pool2 rw - btrfs /dev/sdd1 rw
";
        assert_eq!(
            covered_flags(&parse(text.as_bytes())),
            vec![false, false, true]
        );
        // With the second top-level mount first in the table it is the one
        // that is entered, `/pool` is the repeat, and `home` is a plain folder
        // of `/pool2` (it is not in that mount's way): `/pool/home` is covered.
        let text = "\
3 0 0:27 / /pool2 rw - btrfs /dev/sdd1 rw
1 3 0:27 / /pool rw - btrfs /dev/sdd1 rw
2 1 0:27 /home /pool/home rw - btrfs /dev/sdd1 rw
";
        assert_eq!(
            covered_flags(&parse(text.as_bytes())),
            vec![false, true, true]
        );
        // A subvolume shown again by a covering mount that is entered stays
        // covered: /data shows @/q at /data/q, mount 2 is elsewhere.
        let text = "\
1 0 0:27 /@ /data rw - btrfs /dev/sda3 rw
2 1 0:27 /@/q /data/elsewhere rw - btrfs /dev/sda3 rw
3 1 0:27 /@/q/r /data/q/r rw - btrfs /dev/sda3 rw
";
        assert_eq!(
            covered_flags(&parse(text.as_bytes())),
            vec![false, true, false]
        );
    }

    #[test]
    fn a_subvolume_mounted_on_its_own_folder_is_never_covered_by_the_top_level_or_its_parents() {
        // The layout of a system installed on the top level of the filesystem:
        // `/` is subvolume 5, `home` is mounted on `/home`, and a subvolume of
        // `home` on the folder where `home` shows it, each with the options a
        // real table carries.
        let text = "\
29 1 0:27 / / rw,relatime shared:1 - btrfs /dev/sda3 rw,ssd,subvolid=5,subvol=/
41 29 0:27 /home /home rw,relatime shared:25 - btrfs /dev/sda3 rw,ssd,subvolid=257,subvol=/home
57 41 0:27 /home/alice /home/alice rw,relatime shared:31 - btrfs /dev/sda3 rw,ssd,subvolid=300,subvol=/home/alice
";
        assert_eq!(
            covered_flags(&parse(text.as_bytes())),
            vec![false, false, false]
        );
        // Mounted in the opposite order of the table, the answer is the same.
        let text = "\
57 41 0:27 /home/alice /home/alice rw - btrfs /dev/sda3 rw,subvol=/home/alice
41 29 0:27 /home /home rw - btrfs /dev/sda3 rw,subvol=/home
29 1 0:27 / / rw - btrfs /dev/sda3 rw,subvol=/
";
        assert_eq!(
            covered_flags(&parse(text.as_bytes())),
            vec![false, false, false]
        );
    }

    #[test]
    fn a_mount_covered_by_a_mount_that_is_covered_itself_is_covered_once_and_counted_once() {
        // `/data` shows the subvolume `@`. Mount 2 shows `@/q` on `/data/e`,
        // which `/data` shows at `/data/q` already: a repeat. Mount 3 shows
        // `@/q/r` on `/data/e2`, inside what both `/data` and mount 2 show.
        // Mount 2 is a repeat, so it shows nothing and covers nothing; mount
        // 3 is covered by `/data`. Both are flagged, and the files under
        // `@/q/r` are reached once, through `/data`.
        let text = "\
1 0 0:27 /@ /data rw - btrfs /dev/sda3 rw
2 1 0:27 /@/q /data/e rw - btrfs /dev/sda3 rw
3 1 0:27 /@/q/r /data/e2 rw - btrfs /dev/sda3 rw
";
        assert_eq!(
            covered_flags(&parse(text.as_bytes())),
            vec![false, true, true]
        );
    }

    #[test]
    fn the_top_level_btrfs_mount_covers_the_subvolume_mounts() {
        // `/data` shows the top of the filesystem, where @, @home and the rest
        // are ordinary folders, so a walk of it already reaches them.
        let text = include_str!("../../tests/fixtures/mountinfo/btrfs-subvolumes.txt");
        let candidates = plan(parse(text.as_bytes()), &removable(&[]));
        let btrfs = candidates
            .iter()
            .find(|c| c.entry.fs_type == "btrfs")
            .unwrap();
        assert_eq!(btrfs.members[0].mount_point, b"/data");
        let flags = covered_flags(&btrfs.members);
        assert!(!flags[0]);
        assert!(flags[1..].iter().all(|f| *f));
    }

    #[test]
    fn a_subvolume_mounted_on_its_own_folder_is_not_covered_by_the_top_level() {
        // The top level of the filesystem (subvolume 5) is `/`; the subvolume
        // `home` is a folder of it and is also mounted on that very folder.
        let text = "\
29 1 0:27 / / rw - btrfs /dev/sda3 rw,subvolid=5,subvol=/
41 29 0:27 /home /home rw - btrfs /dev/sda3 rw,subvolid=257,subvol=/home
42 29 0:27 /var/log /var/log rw - btrfs /dev/sda3 rw,subvolid=258,subvol=/var/log
43 29 0:27 /home/alice/src /srv/src rw - btrfs /dev/sda3 rw,subvolid=259,subvol=/home/alice/src
44 29 0:27 /home /mnt/home-again rw - btrfs /dev/sda3 rw,subvolid=257,subvol=/home
";
        let entries = parse(text.as_bytes());
        let points: Vec<_> = entries
            .iter()
            .map(|e| String::from_utf8_lossy(&e.mount_point).into_owned())
            .collect();
        assert_eq!(
            points,
            ["/", "/home", "/var/log", "/srv/src", "/mnt/home-again"]
        );
        // `/home` and `/var/log` are mounted on the folders where `/` shows
        // them, so the top level does not cover them. `/srv/src` shows a
        // folder that the other two show already, and `/mnt/home-again` shows
        // `/home` a second time: both are repeats.
        assert_eq!(
            covered_flags(&entries),
            vec![false, false, false, true, true]
        );
    }

    #[test]
    fn mounts_that_are_not_in_the_way_of_what_covers_them_are_still_covered() {
        // A subvolume mounted deeper than its own folder is still reachable
        // through the top level.
        let text = "\
1 0 0:27 / / rw - btrfs /dev/sda3 rw
2 1 0:27 /data /srv/data rw - btrfs /dev/sda3 rw
";
        assert_eq!(covered_flags(&parse(text.as_bytes())), vec![false, true]);
        // The top level mounted below the others covers them where they are
        // not in its way.
        let text = "\
1 0 0:27 /@ / rw - btrfs /dev/sda3 rw
2 1 0:27 /@home /home rw - btrfs /dev/sda3 rw
3 1 0:27 / /data rw - btrfs /dev/sda3 rw
";
        assert_eq!(
            covered_flags(&parse(text.as_bytes())),
            vec![true, true, false]
        );
    }

    #[test]
    fn two_mounts_of_the_same_folder_cover_only_the_later_one() {
        let text = "\
1 0 8:1 / /a rw - ext4 /dev/sda1 rw
2 0 8:1 / /b rw - ext4 /dev/sda1 rw
3 0 8:1 /x /c rw - ext4 /dev/sda1 rw
";
        let entries = parse(text.as_bytes());
        assert_eq!(covered_flags(&entries), vec![false, true, true]);
    }

    #[test]
    fn is_under_matches_whole_components() {
        assert!(is_under(b"/home/a", b"/"));
        assert!(is_under(b"/home/a", b"/home"));
        assert!(is_under(b"/home", b"/home"));
        assert!(!is_under(b"/homework", b"/home"));
        assert!(!is_under(b"relative", b"/"));
    }

    #[test]
    fn find_prefers_the_device_then_the_longest_prefix() {
        let text = include_str!("../../tests/fixtures/mountinfo/ubuntu-desktop.txt");
        let candidates = plan(parse(text.as_bytes()), &removable(&[]));
        let mount_of = |dev, path: &[u8]| {
            find(&candidates, dev, path)
                .map(|c| String::from_utf8_lossy(&c.entry.mount_point).into_owned())
        };
        assert_eq!(mount_of((8, 4), b"/home/alice/x").as_deref(), Some("/home"));
        // Through the bind mount: same device, same drive.
        assert_eq!(
            mount_of((8, 4), b"/srv/projects/x").as_deref(),
            Some("/home")
        );
        assert_eq!(mount_of((8, 3), b"/etc/passwd").as_deref(), Some("/"));
        // A tmpfs folder resolves to its own hidden candidate.
        assert_eq!(mount_of((0, 45), b"/tmp/scratch").as_deref(), Some("/tmp"));
        // A device number from another mount namespace falls back to the path.
        assert_eq!(
            mount_of((9, 9), b"/data/projects").as_deref(),
            Some("/data")
        );
        assert_eq!(mount_of((9, 9), b"relative"), None);
    }

    #[test]
    fn every_mount_of_one_btrfs_filesystem_has_the_same_device_number() {
        // As the kernel prints them: one superblock, one major:minor pair.
        let text = include_str!("../../tests/fixtures/mountinfo/btrfs-subvolumes.txt");
        let entries = parse(text.as_bytes());
        let numbers: std::collections::HashSet<_> = entries
            .iter()
            .filter(|e| e.fs_type == "btrfs")
            .map(|e| (e.major, e.minor))
            .collect();
        assert_eq!(numbers.len(), 1);
        // A lookup by that number alone cannot tell the subvolumes apart, so the
        // longest mount-point prefix picks the mount; the drive is the same.
        let candidates = plan(entries, &removable(&[]));
        let (major, minor) = *numbers.iter().next().unwrap();
        let home = find(&candidates, (major, minor), b"/home/alice").unwrap();
        let log = find(&candidates, (major, minor), b"/var/log/syslog").unwrap();
        assert_eq!(home.entry.mount_point, log.entry.mount_point);
    }

    #[test]
    fn btrfs_subvolume_path_with_an_unknown_device_number_still_finds_the_drive() {
        // `stat` on a subvolume folder can report a device number that the
        // mount table does not list; the longest mount-point prefix decides.
        let text = include_str!("../../tests/fixtures/mountinfo/btrfs-subvolumes.txt");
        let candidates = plan(parse(text.as_bytes()), &removable(&[]));
        let found = find(&candidates, (0, 99), b"/home/alice/docs").unwrap();
        assert_eq!(found.entry.fs_type, "btrfs");
        let boot = find(&candidates, (0, 99), b"/boot/grub").unwrap();
        assert_eq!(boot.entry.fs_type, "ext4");
    }

    #[test]
    fn hidden_candidates_are_kept_for_path_lookups() {
        let text = include_str!("../../tests/fixtures/mountinfo/ubuntu-desktop.txt");
        let candidates = plan(parse(text.as_bytes()), &removable(&[]));
        let tmp = candidates
            .iter()
            .find(|c| c.entry.mount_point == b"/tmp")
            .unwrap();
        assert!(tmp.hidden);
        assert_eq!(tmp.kind, DriveKind::Other);
    }
}
