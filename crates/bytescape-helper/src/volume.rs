//! Telling a disk of this computer from a network volume.
//!
//! The helper reads and deletes with administrator rights, and only on disks
//! of this computer. A network volume is the other computer's: what its server
//! returns to an administrator account is not for this helper to act on, a
//! delete there is permanent for whoever else shares it, and a reader that
//! blocks on a stuck server holds the elevated process open. A request on one
//! is refused outright.
//!
//! The answers come from the helper's own view of the system, never from the
//! request: on Linux from the mount table (and the kernel's file system
//! number for a second opinion), on Windows from the drive type and the shape
//! of the path. Where the helper cannot tell, the answer is
//! [`Verdict::Unknown`], which is treated as a refusal too.

#![allow(unsafe_code)]

/// What kind of volume a path is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// A disk of this computer (fixed, removable, optical or a RAM disk).
    Local,
    /// A network share or a remote file system.
    Remote,
    /// The helper cannot tell.
    Unknown,
}

/// File system types of the mount table that are on another computer.
const NETWORK_FILESYSTEMS: &[&str] = &[
    "nfs",
    "nfs4",
    "cifs",
    "smb3",
    "smb2",
    "smbfs",
    "smb",
    "ncpfs",
    "ncp",
    "afs",
    "coda",
    "9p",
    "ceph",
    "glusterfs",
    "lustre",
    "beegfs",
    "orangefs",
    "panfs",
    "gpfs",
    "davfs",
    "davfs2",
    "sshfs",
    "curlftpfs",
    "ftpfs",
    "afpfs",
    "ocfs2_dlmfs",
    "pvfs2",
    "ibrix",
    "nfsd",
];

/// Programs that serve a network volume through FUSE, as they name themselves
/// after `fuse.` in the mount table.
const NETWORK_FUSE: &[&str] = &[
    "sshfs",
    "rclone",
    "s3fs",
    "gcsfuse",
    "goofys",
    "mountpoint-s3",
    "curlftpfs",
    "davfs2",
    "glusterfs",
    "ceph-fuse",
    "cephfs",
    "gvfsd-fuse",
    "onedriver",
    "google-drive-ocamlfuse",
    "smbnetfs",
    "afpfs",
    "juicefs",
    "mfsmount",
    "seaweedfs",
    "blobfuse",
    "blobfuse2",
    "httpdirfs",
];

/// Whether a file system type of the mount table (`nfs4`, `cifs`,
/// `fuse.sshfs`, ...) is a network file system. Names are compared without
/// regard to case.
pub fn is_network_filesystem(fs_type: &str) -> bool {
    let lower = fs_type.to_ascii_lowercase();
    if NETWORK_FILESYSTEMS.contains(&lower.as_str()) {
        return true;
    }
    // `fuse.sshfs`, and the same under `fuseblk`-style subtypes.
    lower
        .split_once('.')
        .is_some_and(|(kind, program)| kind == "fuse" && NETWORK_FUSE.contains(&program))
}

/// File system numbers (`f_type` of `statfs`) of network file systems. The
/// kernel's own word for what a folder is on, used besides the mount table.
#[cfg(target_os = "linux")]
const NETWORK_MAGIC: &[i64] = &[
    0x6969,      // NFS
    0x517B,      // SMB
    0xFF53_4D42, // CIFS
    0xFE53_4D42, // SMB2 and later
    0x0102_1997, // 9P
    0x00C3_6400, // Ceph
    0x5346_414F, // AFS
    0x7375_7245, // Coda
    0x564C,      // NCP
];

/// Whether a `statfs` file system number belongs to a network file system.
#[cfg(target_os = "linux")]
pub fn is_network_magic(f_type: i64) -> bool {
    NETWORK_MAGIC.contains(&f_type)
}

/// The verdict for a place on Linux, from the mount table, the mount ID the
/// kernel gave the item, and (if known) the kernel's file system number.
///
/// The mount is found by its ID when there is one: a path can be hidden by a
/// mount placed over a folder above it, but the ID names the mount the item is
/// really on. Without an ID the mount that holds the path in the table is used.
#[cfg(target_os = "linux")]
pub fn linux_verdict(
    mounts: &[crate::mounts::Mount],
    mount_id: Option<u64>,
    path: &[u8],
    magic: Option<i64>,
) -> Verdict {
    if magic.is_some_and(is_network_magic) {
        return Verdict::Remote;
    }
    let mount = match mount_id {
        Some(id) => mounts.iter().find(|m| m.id == id),
        None => crate::mounts::containing(mounts, path),
    };
    match mount {
        Some(m) if is_network_filesystem(&m.fs_type) => Verdict::Remote,
        Some(_) => Verdict::Local,
        None => Verdict::Unknown,
    }
}

/// A scan root the helper has judged to be on a disk of this computer, and
/// what holds the scan to that very folder.
pub(crate) struct Judged {
    /// The judged folder, held open. Every reader checks the root it opened
    /// against it before it reads or sends anything, so a scan can only run
    /// on what was judged. It must live until the scan has ended.
    pub pin: bs_scan::RootPin,
    /// The mount the judged folder is on, as the kernel numbers it (Linux
    /// 5.8 and later), taken from the held descriptor. It places the scan in
    /// the mount table.
    pub mount_id: Option<u64>,
    /// Bytes the operating system reports in use on the judged folder's
    /// volume, for the scan's progress. On Linux it is asked of the held
    /// descriptor.
    pub os_used: Option<u64>,
}

/// Why a scan root was not judged fit for an administrator scan.
#[derive(Debug)]
pub(crate) enum NotJudged {
    /// The volume is on another computer.
    Remote,
    /// The helper cannot tell what the volume is, or cannot open the folder
    /// as a folder.
    Unknown,
    /// The volume is local, but the scan cannot be held to the folder that
    /// was judged. `Unsupported` means this system gives no way to hold it
    /// (Linux before 5.8); any other error is the system's own refusal.
    CannotPin(std::io::Error),
}

/// Judges the folder `root` of a scan, on this computer, and makes the pin
/// that holds the scan to it.
///
/// The folder is opened once, as a folder and without following a link. The
/// verdict (the mount, and the kernel's file system number) is taken from that
/// descriptor, and the pin is made from the same descriptor. The root's name
/// is attacker-reachable (a folder above it can be swapped after the caller
/// looked), so it is never opened a second time for the verdict, the plan or
/// the pin, and a name that has become a pipe or a device is never opened at
/// all.
#[cfg(target_os = "linux")]
pub(crate) fn judge_scan_root(
    root: &std::path::Path,
    host: &crate::scan::Host,
) -> Result<Judged, NotJudged> {
    use std::os::fd::{AsFd, AsRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    let folder = crate::plan::open_scan_root(root).ok_or(NotJudged::Unknown)?;
    let held: OwnedFd = folder.into();
    // The table is read after the folder is open: the open folder keeps its
    // mount ID from being given to another mount, so the table that is read
    // next cannot list a different mount under that number.
    let text = std::fs::read(&host.mountinfo).map_err(|_| NotJudged::Unknown)?;
    let mount_id = crate::plan::mount_id_of_fd(held.as_fd());
    let os_used = bs_fsops::remove::fstatvfs_used(held.as_raw_fd()).ok();
    let mounts = crate::mounts::parse(&text);
    match linux_verdict(
        &mounts,
        mount_id,
        root.as_os_str().as_bytes(),
        bs_fsops::remove::fstatfs_type(held.as_raw_fd()).ok(),
    ) {
        Verdict::Local => {}
        Verdict::Remote => return Err(NotJudged::Remote),
        Verdict::Unknown => return Err(NotJudged::Unknown),
    }
    let pin = bs_scan::RootPin::from_fd(std::sync::Arc::new(held)).map_err(NotJudged::CannotPin)?;
    Ok(Judged {
        pin,
        mount_id,
        os_used,
    })
}

/// Judges the folder `root` of a scan, on this computer, and makes the pin
/// that holds the scan to it.
///
/// The folder is opened once for its attributes, as itself (a link or
/// junction is not followed), and both the verdict and the pin come from that
/// handle: the verdict from the place the handle really is, the pin from the
/// handle's volume serial number and file ID.
#[cfg(windows)]
pub(crate) fn judge_scan_root(
    root: &std::path::Path,
    _host: &crate::scan::Host,
) -> Result<Judged, NotJudged> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::os::windows::io::{AsRawHandle, OwnedHandle};
    const FILE_READ_ATTRIBUTES: u32 = 0x80;
    const SHARE_ALL: u32 = 7;
    const BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    let file = std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(SHARE_ALL)
        .custom_flags(BACKUP_SEMANTICS | OPEN_REPARSE_POINT)
        .open(root)
        .map_err(|_| NotJudged::Unknown)?;
    let meta = file.metadata().map_err(|_| NotJudged::Unknown)?;
    // The root was resolved to its real place before it got here; a folder
    // that is a link now has been swapped in, and is not scanned.
    if !meta.is_dir() || meta.file_attributes() & ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(NotJudged::Unknown);
    }
    // Where this handle really is: a share is remote whatever its name looks
    // like. The wrapper only borrows the handle; the file closes it.
    let borrowed = std::mem::ManuallyDrop::new(bs_fsops::win::Handle(file.as_raw_handle()));
    let placed = bs_fsops::win::final_path(&borrowed).map_err(|_| NotJudged::Unknown)?;
    match SystemDrives::verdict(std::path::Path::new(&placed)) {
        Verdict::Local => {}
        Verdict::Remote => return Err(NotJudged::Remote),
        Verdict::Unknown => return Err(NotJudged::Unknown),
    }
    let pin = bs_scan::RootPin::from_handle(std::sync::Arc::new(OwnedHandle::from(file)))
        .map_err(NotJudged::CannotPin)?;
    // A figure for the progress of the scan, not a decision.
    let os_used = bs_scan::VolumeInfo::used_bytes(&bs_scan::StatvfsVolume, root).ok();
    Ok(Judged {
        pin,
        mount_id: None,
        os_used,
    })
}

/// Judges the folder `root` of a scan, on a system the helper does not
/// support.
#[cfg(not(any(target_os = "linux", windows)))]
pub(crate) fn judge_scan_root(
    _root: &std::path::Path,
    _host: &crate::scan::Host,
) -> Result<Judged, NotJudged> {
    Err(NotJudged::Unknown)
}

/// The verdict for a place a delete is about to act in, on this computer.
#[cfg(target_os = "linux")]
pub(crate) fn of_delete_item(
    real: &std::path::Path,
    mount_id: Option<u64>,
    magic: Option<i64>,
    mountinfo: &std::path::Path,
) -> Verdict {
    use std::os::unix::ffi::OsStrExt;
    let Ok(text) = std::fs::read(mountinfo) else {
        return Verdict::Unknown;
    };
    let mounts = crate::mounts::parse(&text);
    // `magic` is the held parent folder's answer: it says what the item is on
    // (the item itself is never a mount point, which is refused separately).
    linux_verdict(&mounts, mount_id, real.as_os_str().as_bytes(), magic)
}

/// How a delete asks whether an item is on a disk of this computer.
///
/// The question is asked twice. Before the item is opened only its path is
/// known, and only a clear "remote" is acted on: opening an item on a
/// network volume can itself block on a server that has stopped answering.
/// After it is opened the answer must be "local": a volume the helper cannot
/// place is refused too.
pub trait VolumeCheck: Send + Sync {
    /// From the path as it was sent, without touching the disk.
    fn before_opening(&self, path: &std::path::Path) -> Verdict;
    /// From the place the item really is, the mount the kernel says it is on,
    /// and the kernel's file system number for the folder holding it (Linux).
    /// Both numbers come from descriptors already held, never from a name
    /// looked up again.
    fn after_opening(
        &self,
        real: &std::path::Path,
        mount_id: Option<u64>,
        filesystem_magic: Option<i64>,
    ) -> Verdict;
}

/// The check for this computer's mount table (Linux), read afresh each time.
#[cfg(target_os = "linux")]
pub struct MountTable {
    mountinfo: std::path::PathBuf,
}

#[cfg(target_os = "linux")]
impl MountTable {
    /// A check that reads the mount table at `mountinfo`
    /// (`/proc/self/mountinfo`).
    pub fn new(mountinfo: impl Into<std::path::PathBuf>) -> Self {
        Self {
            mountinfo: mountinfo.into(),
        }
    }
}

#[cfg(target_os = "linux")]
impl VolumeCheck for MountTable {
    fn before_opening(&self, path: &std::path::Path) -> Verdict {
        use std::os::unix::ffi::OsStrExt;
        let Ok(text) = std::fs::read(&self.mountinfo) else {
            return Verdict::Unknown;
        };
        let mounts = crate::mounts::parse(&text);
        match linux_verdict(&mounts, None, path.as_os_str().as_bytes(), None) {
            Verdict::Remote => Verdict::Remote,
            // Not yet known: the mount of the opened item decides.
            _ => Verdict::Unknown,
        }
    }

    fn after_opening(
        &self,
        real: &std::path::Path,
        mount_id: Option<u64>,
        filesystem_magic: Option<i64>,
    ) -> Verdict {
        of_delete_item(real, mount_id, filesystem_magic, &self.mountinfo)
    }
}

/// The check for this computer's drives (Windows).
#[cfg(windows)]
pub struct SystemDrives;

#[cfg(windows)]
impl SystemDrives {
    pub(crate) fn verdict(path: &std::path::Path) -> Verdict {
        use std::os::windows::ffi::OsStrExt;
        let units: Vec<u16> = path.as_os_str().encode_wide().collect();
        windows_verdict(&units, crate::windows::drive_type_of_root)
    }
}

#[cfg(windows)]
impl VolumeCheck for SystemDrives {
    fn before_opening(&self, path: &std::path::Path) -> Verdict {
        match Self::verdict(path) {
            Verdict::Remote => Verdict::Remote,
            _ => Verdict::Unknown,
        }
    }

    fn after_opening(
        &self,
        real: &std::path::Path,
        _mount_id: Option<u64>,
        _filesystem_magic: Option<i64>,
    ) -> Verdict {
        Self::verdict(real)
    }
}

// ------------------------------------------------------------------ Windows

/// How a Windows path names its volume.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WindowsVolume {
    /// A network share: `\\server\share`, `\\?\UNC\server\share`.
    Share,
    /// A drive letter, as the root `X:\`.
    Letter(char),
    /// A volume by its GUID, as the root `\\?\Volume{...}\`.
    Guid(Vec<u16>),
    /// Anything else: a device name, a relative path, an odd spelling.
    Other,
}

const BACKSLASH: u16 = b'\\' as u16;
const SLASH: u16 = b'/' as u16;

fn lower(unit: u16) -> u16 {
    match u8::try_from(unit) {
        Ok(byte) => u16::from(byte.to_ascii_lowercase()),
        Err(_) => unit,
    }
}

fn starts_with_ignoring_case(units: &[u16], prefix: &str) -> bool {
    let mut want = prefix.encode_utf16();
    let mut have = units.iter();
    loop {
        match (want.next(), have.next()) {
            (None, _) => return true,
            (Some(w), Some(&h)) if lower(w) == lower(h) => {}
            _ => return false,
        }
    }
}

/// Which volume a Windows path is on, from its spelling alone.
pub fn windows_volume(path: &[u16]) -> WindowsVolume {
    // Both separators mean the same.
    let path: Vec<u16> = path
        .iter()
        .map(|&u| if u == SLASH { BACKSLASH } else { u })
        .collect();
    if starts_with_ignoring_case(&path, r"\\?\UNC\")
        || starts_with_ignoring_case(&path, r"\??\UNC\")
    {
        return WindowsVolume::Share;
    }
    let after_prefix: &[u16] =
        if starts_with_ignoring_case(&path, r"\\?\") || starts_with_ignoring_case(&path, r"\??\") {
            path.get(4..).unwrap_or_default()
        } else if starts_with_ignoring_case(&path, r"\\.\") {
            return WindowsVolume::Other;
        } else if path.starts_with(&[BACKSLASH, BACKSLASH]) {
            // `\\server\share`: the only other thing a double backslash starts.
            return WindowsVolume::Share;
        } else {
            &path
        };
    match after_prefix {
        [letter, colon, rest @ ..]
            if *colon == u16::from(b':')
                && (rest.is_empty() || rest.first() == Some(&BACKSLASH))
                && u8::try_from(*letter).is_ok_and(|b| b.is_ascii_alphabetic()) =>
        {
            match u8::try_from(*letter) {
                Ok(b) => WindowsVolume::Letter(char::from(b.to_ascii_uppercase())),
                Err(_) => WindowsVolume::Other,
            }
        }
        _ if starts_with_ignoring_case(after_prefix, "Volume{") => {
            let end = after_prefix
                .iter()
                .position(|&u| u == u16::from(b'}'))
                .and_then(|at| at.checked_add(1));
            match end.and_then(|end| after_prefix.get(..end)) {
                Some(name) => {
                    let mut root: Vec<u16> = r"\\?\".encode_utf16().collect();
                    root.extend_from_slice(name);
                    root.push(BACKSLASH);
                    WindowsVolume::Guid(root)
                }
                None => WindowsVolume::Other,
            }
        }
        _ => WindowsVolume::Other,
    }
}

/// `GetDriveTypeW` answers.
pub mod drive_type {
    pub const UNKNOWN: u32 = 0;
    pub const NO_ROOT_DIR: u32 = 1;
    pub const REMOTE: u32 = 4;
}

/// The verdict for a Windows path. `drive_type` is asked for the root of the
/// volume as a NUL-free string of units (`C:\`, or `\\?\Volume{...}\`) and
/// answers as `GetDriveTypeW` does.
pub fn windows_verdict(path: &[u16], drive_type: impl Fn(&[u16]) -> u32) -> Verdict {
    let root: Vec<u16> = match windows_volume(path) {
        WindowsVolume::Share => return Verdict::Remote,
        WindowsVolume::Letter(letter) => format!("{letter}:\\").encode_utf16().collect(),
        WindowsVolume::Guid(root) => root,
        WindowsVolume::Other => return Verdict::Unknown,
    };
    match drive_type(&root) {
        drive_type::REMOTE => Verdict::Remote,
        drive_type::UNKNOWN | drive_type::NO_ROOT_DIR => Verdict::Unknown,
        _ => Verdict::Local,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
    use super::*;

    fn u(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    #[test]
    fn network_file_systems_are_known_by_the_name_the_mount_table_gives() {
        for remote in [
            "nfs",
            "nfs4",
            "NFS4",
            "cifs",
            "smb3",
            "9p",
            "ceph",
            "glusterfs",
            "fuse.sshfs",
            "fuse.rclone",
            "FUSE.SSHFS",
            "fuse.gvfsd-fuse",
            "davfs",
            "afs",
        ] {
            assert!(is_network_filesystem(remote), "{remote}");
        }
        for local in [
            "ext4",
            "xfs",
            "btrfs",
            "vfat",
            "exfat",
            "ntfs3",
            "fuseblk",
            "fuse",
            "fuse.ntfs-3g",
            "fuse.encfs",
            "tmpfs",
            "overlay",
            "zfs",
            "",
            "nfsx",
            "sshfs2",
            "fuse.",
            "fuse.sshfs.evil",
        ] {
            assert!(!is_network_filesystem(local), "{local:?}");
        }
    }

    #[cfg(target_os = "linux")]
    mod linux {
        use super::*;
        use crate::mounts::parse;

        const TABLE: &str = "\
1 0 8:1 / / rw - ext4 /dev/sda1 rw
2 1 0:40 / /mnt/share rw - cifs //srv/share rw
3 1 0:41 / /mnt/nfs rw - nfs4 srv:/export rw
4 1 0:42 / /home/me/remote rw - fuse.sshfs me@host:/ rw
5 1 8:2 / /mnt/disk rw - xfs /dev/sdb1 rw
6 5 0:43 / /mnt/disk/cloud rw - fuse.rclone remote: rw
7 1 0:44 / /mnt/hidden rw - nfs4 srv:/a rw
";

        /// The same table with a tmpfs later mounted over `/mnt`, hiding the
        /// mounts below it from anyone who goes by the path.
        const OVERMOUNTED: &str = "\
1 0 8:1 / / rw - ext4 /dev/sda1 rw
2 1 0:40 / /mnt/share rw - cifs //srv/share rw
3 1 0:41 / /mnt/nfs rw - nfs4 srv:/export rw
4 1 0:42 / /home/me/remote rw - fuse.sshfs me@host:/ rw
5 1 8:2 / /mnt/disk rw - xfs /dev/sdb1 rw
6 5 0:43 / /mnt/disk/cloud rw - fuse.rclone remote: rw
7 1 0:44 / /mnt/hidden rw - nfs4 srv:/a rw
8 1 0:45 / /mnt hw - tmpfs tmpfs rw
";

        fn verdict_in(table: &str, id: Option<u64>, path: &str, magic: Option<i64>) -> Verdict {
            linux_verdict(&parse(table.as_bytes()), id, path.as_bytes(), magic)
        }

        fn verdict(id: Option<u64>, path: &str, magic: Option<i64>) -> Verdict {
            verdict_in(OVERMOUNTED, id, path, magic)
        }

        #[test]
        fn a_path_is_judged_by_the_mount_that_holds_it() {
            for (path, want) in [
                ("/", Verdict::Local),
                ("/etc/passwd", Verdict::Local),
                ("/mnt/share", Verdict::Remote),
                ("/mnt/share/deep/er", Verdict::Remote),
                ("/mnt/nfs/x", Verdict::Remote),
                ("/home/me/remote/x", Verdict::Remote),
                ("/mnt/disk/data", Verdict::Local),
                ("/mnt/disk/cloud/file", Verdict::Remote),
                ("/mnt/hidden/x", Verdict::Remote),
                // A name that only starts like a mount point is not inside it.
                ("/mnt/sharex", Verdict::Local),
            ] {
                // Without an ID the table's own answer is used.
                assert_eq!(verdict_in(TABLE, None, path, None), want, "{path}");
            }
        }

        #[test]
        fn a_mount_over_a_folder_hides_the_mounts_below_it_from_the_path() {
            // With a tmpfs over `/mnt` the path alone calls the places below
            // it local: this is why a delete asks the kernel's mount ID too.
            for path in ["/mnt/share", "/mnt/nfs/x", "/mnt/hidden/x", "/mnt/x"] {
                assert_eq!(verdict(None, path, None), Verdict::Local, "{path}");
            }
            // Places outside it are unaffected.
            assert_eq!(verdict(None, "/home/me/remote/x", None), Verdict::Remote);
            assert_eq!(verdict(None, "/etc", None), Verdict::Local);
            // And the ID still finds the mount the item is on.
            assert_eq!(verdict(Some(2), "/mnt/share/x", None), Verdict::Remote);
        }

        #[test]
        fn the_mount_id_names_the_mount_the_item_is_really_on() {
            // 7 is a network mount that a later tmpfs over `/mnt` hides: the
            // path says tmpfs, the ID says nfs4, and the ID is right.
            assert_eq!(verdict(Some(7), "/mnt/hidden/x", None), Verdict::Remote);
            assert_eq!(verdict(Some(8), "/mnt/hidden/x", None), Verdict::Local);
            assert_eq!(verdict(Some(1), "/mnt/share/x", None), Verdict::Local);
            assert_eq!(verdict(Some(2), "/anything", None), Verdict::Remote);
            assert_eq!(verdict(Some(4), "/anything", None), Verdict::Remote);
        }

        #[test]
        fn what_the_table_cannot_place_is_unknown_and_not_local() {
            assert_eq!(verdict(Some(999), "/x", None), Verdict::Unknown);
            assert_eq!(verdict(None, "relative", None), Verdict::Unknown);
            assert_eq!(
                linux_verdict(&[], Some(1), b"/", None),
                Verdict::Unknown,
                "an empty table"
            );
        }

        #[test]
        fn the_kernels_file_system_number_is_a_second_opinion() {
            // The table says ext4; the kernel says the folder is on NFS.
            for magic in [0x6969, 0x517B, 0xFF53_4D42, 0xFE53_4D42, 0x0102_1997] {
                assert_eq!(verdict(Some(1), "/", Some(magic)), Verdict::Remote);
            }
            // EXT4 and tmpfs numbers do not make a volume remote.
            for magic in [0xEF53, 0x0102_1994, 0x9123_683E] {
                assert_eq!(verdict(Some(1), "/", Some(magic)), Verdict::Local);
            }
        }

        #[test]
        fn a_real_local_folder_is_local() {
            let dir = std::fs::canonicalize(std::env::temp_dir()).unwrap();
            let host = crate::scan::Host::system();
            // Whatever this machine's temporary folder is on, it is not remote
            // (the tests refuse to run in a network home).
            assert!(!matches!(
                judge_scan_root(&dir, &host),
                Err(NotJudged::Remote)
            ));
            let missing = crate::scan::Host {
                mountinfo: "/nonexistent/mountinfo".into(),
            };
            assert!(matches!(
                judge_scan_root(&dir, &missing),
                Err(NotJudged::Unknown)
            ));
        }

        /// A scratch folder made by one test, removed when it is dropped. Only
        /// the folder this made is ever removed.
        struct Scratch(std::path::PathBuf);

        impl Scratch {
            fn new(name: &str) -> Self {
                let dir = std::env::temp_dir()
                    .join(format!("bytescape-volume-{}-{name}", std::process::id()));
                let _ = std::fs::remove_dir_all(&dir);
                std::fs::create_dir_all(&dir).unwrap();
                Self(std::fs::canonicalize(dir).unwrap())
            }
        }

        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        fn c_path(path: &std::path::Path) -> std::ffi::CString {
            use std::os::unix::ffi::OsStrExt;
            std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap()
        }

        /// A thread that blocks opening the named pipe for writing, and
        /// notes it if a reader ever appears. Opening a pipe for reading
        /// without `O_NONBLOCK` would block too; with it, the open succeeds
        /// and wakes this thread, which is how the tests see that the helper
        /// opened the pipe at all.
        struct PipeWatcher {
            woken: std::sync::Arc<std::sync::atomic::AtomicBool>,
            thread: std::thread::JoinHandle<()>,
            fifo: std::path::PathBuf,
        }

        impl PipeWatcher {
            fn start(fifo: &std::path::Path) -> Self {
                use std::sync::atomic::Ordering;
                let woken = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let (flag, path) = (woken.clone(), fifo.to_path_buf());
                let thread = std::thread::spawn(move || {
                    // SAFETY: a NUL-terminated path that outlives the call.
                    let fd = unsafe { libc::open(c_path(&path).as_ptr(), libc::O_WRONLY) };
                    flag.store(true, Ordering::SeqCst);
                    if fd >= 0 {
                        // SAFETY: `fd` was just opened here and is closed once.
                        unsafe { libc::close(fd) };
                    }
                });
                // Time for the thread to reach the blocking open.
                std::thread::sleep(std::time::Duration::from_millis(100));
                Self {
                    woken,
                    thread,
                    fifo: fifo.to_path_buf(),
                }
            }

            /// Whether anything opened the pipe for reading since it started.
            fn was_opened(&self) -> bool {
                std::thread::sleep(std::time::Duration::from_millis(100));
                self.woken.load(std::sync::atomic::Ordering::SeqCst)
            }

            /// Lets the thread finish if nothing woke it.
            fn finish(self) {
                if !self.woken.load(std::sync::atomic::Ordering::SeqCst) {
                    // SAFETY: a NUL-terminated path that outlives the call.
                    let fd = unsafe {
                        libc::open(
                            c_path(&self.fifo).as_ptr(),
                            libc::O_RDONLY | libc::O_NONBLOCK,
                        )
                    };
                    let _ = self.thread.join();
                    if fd >= 0 {
                        // SAFETY: `fd` was opened above and is closed once.
                        unsafe { libc::close(fd) };
                    }
                } else {
                    let _ = self.thread.join();
                }
            }
        }

        #[test]
        fn a_root_that_is_not_a_folder_is_never_opened() {
            let scratch = Scratch::new("not-a-folder");
            let host = crate::scan::Host::system();
            let fifo = scratch.0.join("pipe");
            // SAFETY: a NUL-terminated path that outlives the call.
            assert_eq!(unsafe { libc::mkfifo(c_path(&fifo).as_ptr(), 0o600) }, 0);
            let watcher = PipeWatcher::start(&fifo);
            assert!(matches!(
                judge_scan_root(&fifo, &host),
                Err(NotJudged::Unknown)
            ));
            assert!(
                !watcher.was_opened(),
                "the check opened a named pipe that was named as the root"
            );
            watcher.finish();
            // A plain file and a link to a folder are not opened as roots
            // either: both are refused as unplaceable.
            let file = scratch.0.join("file");
            std::fs::write(&file, b"x").unwrap();
            assert!(matches!(
                judge_scan_root(&file, &host),
                Err(NotJudged::Unknown)
            ));
            std::fs::create_dir(scratch.0.join("real")).unwrap();
            std::os::unix::fs::symlink(scratch.0.join("real"), scratch.0.join("link")).unwrap();
            assert!(matches!(
                judge_scan_root(&scratch.0.join("link"), &host),
                Err(NotJudged::Unknown)
            ));
            assert!(!matches!(
                judge_scan_root(&scratch.0.join("real"), &host),
                Err(NotJudged::Unknown)
            ));
        }

        /// The mount table is read after the root is open, so the open folder
        /// keeps its mount ID from being given to another mount before the
        /// table that is matched against it is read. The table here is a named
        /// pipe, so the read stays pending while the test looks at the
        /// helper's open files.
        #[test]
        fn the_mount_table_is_read_after_the_root_is_open() {
            let scratch = Scratch::new("table-after-open");
            let root = scratch.0.join("root");
            std::fs::create_dir(&root).unwrap();
            let table = scratch.0.join("mountinfo");
            // SAFETY: a NUL-terminated path that outlives the call.
            assert_eq!(unsafe { libc::mkfifo(c_path(&table).as_ptr(), 0o600) }, 0);
            let judge = {
                let (root, table) = (root.clone(), table.clone());
                std::thread::spawn(move || {
                    let host = crate::scan::Host { mountinfo: table };
                    // Whatever the verdict, only the order is looked at.
                    let _ = judge_scan_root(&root, &host);
                })
            };
            let is_open = || {
                std::fs::read_dir("/proc/self/fd")
                    .unwrap()
                    .flatten()
                    .any(|entry| std::fs::read_link(entry.path()).is_ok_and(|to| to == root))
            };
            let started = std::time::Instant::now();
            let mut open_while_reading = false;
            while started.elapsed() < std::time::Duration::from_secs(3) {
                if is_open() {
                    open_while_reading = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            // Let the read finish, whatever was seen, so the thread ends.
            std::fs::write(&table, std::fs::read("/proc/self/mountinfo").unwrap()).unwrap();
            judge.join().unwrap();
            assert!(
                open_while_reading,
                "the mount table was read before the root was open"
            );
        }

        /// The race this guards: the folder checked as the root has its parent
        /// swapped for a link to a folder holding a named pipe, between the
        /// caller's check and the helper's open. The helper must never open
        /// the pipe.
        #[test]
        fn a_root_whose_parent_is_swapped_for_a_pipe_is_never_opened() {
            use std::sync::atomic::{AtomicBool, Ordering};
            let scratch = Scratch::new("swap-race");
            let base = &scratch.0;
            std::fs::create_dir_all(base.join("a/x")).unwrap();
            std::fs::create_dir_all(base.join("dev")).unwrap();
            let fifo = base.join("dev/x");
            // SAFETY: a NUL-terminated path that outlives the call.
            assert_eq!(unsafe { libc::mkfifo(c_path(&fifo).as_ptr(), 0o600) }, 0);
            std::os::unix::fs::symlink(base.join("dev"), base.join("swap")).unwrap();
            let host = crate::scan::Host::system();
            let watcher = PipeWatcher::start(&fifo);
            let stop = std::sync::Arc::new(AtomicBool::new(false));
            let racer = {
                let (a, swap, stop) = (
                    c_path(&base.join("a")),
                    c_path(&base.join("swap")),
                    stop.clone(),
                );
                std::thread::spawn(move || {
                    while !stop.load(Ordering::SeqCst) {
                        // SAFETY: NUL-terminated paths that outlive the call.
                        unsafe {
                            libc::renameat2(
                                libc::AT_FDCWD,
                                a.as_ptr(),
                                libc::AT_FDCWD,
                                swap.as_ptr(),
                                libc::RENAME_EXCHANGE,
                            )
                        };
                    }
                })
            };
            let want = base.join("a/x");
            let started = std::time::Instant::now();
            let mut checked = 0u32;
            while started.elapsed() < std::time::Duration::from_secs(2)
                && !watcher.woken.load(Ordering::SeqCst)
            {
                // What the caller does: resolve, then require a folder.
                let Ok(real) = std::fs::canonicalize(&want) else {
                    continue;
                };
                if real != want || !real.is_dir() {
                    continue;
                }
                checked += 1;
                let _ = judge_scan_root(&real, &host);
            }
            stop.store(true, Ordering::SeqCst);
            racer.join().unwrap();
            let opened = watcher.was_opened();
            watcher.finish();
            assert!(checked > 0, "the race never produced a checked folder");
            assert!(!opened, "the check opened a pipe that replaced the root");
        }
    }

    #[test]
    fn a_windows_path_names_its_volume_by_its_spelling() {
        for (path, want) in [
            (r"C:\", WindowsVolume::Letter('C')),
            (r"c:\Users\a", WindowsVolume::Letter('C')),
            (r"C:", WindowsVolume::Letter('C')),
            (r"\\?\D:\x", WindowsVolume::Letter('D')),
            (r"\??\E:\x", WindowsVolume::Letter('E')),
            ("c:/x", WindowsVolume::Letter('C')),
            (r"\\server\share\x", WindowsVolume::Share),
            (r"\\server\share", WindowsVolume::Share),
            (r"\\?\UNC\server\share\x", WindowsVolume::Share),
            (r"\\?\unc\server\share", WindowsVolume::Share),
            (r"\??\UNC\server\share", WindowsVolume::Share),
            ("//server/share/x", WindowsVolume::Share),
            (r"\\.\C:", WindowsVolume::Other),
            (r"\\.\GLOBALROOT\Device\X", WindowsVolume::Other),
            (r"relative\path", WindowsVolume::Other),
            (r"", WindowsVolume::Other),
            (r"\x", WindowsVolume::Other),
            (r"C", WindowsVolume::Other),
            (r"CC:\x", WindowsVolume::Other),
            (r"1:\x", WindowsVolume::Other),
            (r"C:x", WindowsVolume::Other),
        ] {
            assert_eq!(windows_volume(&u(path)), want, "{path:?}");
        }
        let guid = windows_volume(&u(r"\\?\Volume{1234-ab}\Users\x"));
        assert_eq!(guid, WindowsVolume::Guid(u(r"\\?\Volume{1234-ab}\")));
        assert_eq!(
            windows_volume(&u(r"\\?\Volume{unterminated\x")),
            WindowsVolume::Other
        );
    }

    #[test]
    fn a_windows_volume_is_remote_when_the_system_says_so_and_unknown_when_it_cannot() {
        use drive_type::{NO_ROOT_DIR, REMOTE, UNKNOWN};
        let asked = std::cell::RefCell::new(Vec::new());
        let answer = |code: u32| {
            let asked = &asked;
            move |root: &[u16]| {
                asked.borrow_mut().push(String::from_utf16_lossy(root));
                code
            }
        };
        // A mapped network drive is a drive letter whose type is remote.
        assert_eq!(
            windows_verdict(&u(r"Z:\x"), answer(REMOTE)),
            Verdict::Remote
        );
        // The other types are disks of this computer.
        for local in [2, 3, 5, 6] {
            assert_eq!(windows_verdict(&u(r"C:\x"), answer(local)), Verdict::Local);
        }
        for unknown in [UNKNOWN, NO_ROOT_DIR] {
            assert_eq!(
                windows_verdict(&u(r"C:\x"), answer(unknown)),
                Verdict::Unknown
            );
        }
        // The system is asked about the root of the volume, in the form it
        // takes: a letter or a GUID.
        asked.borrow_mut().clear();
        windows_verdict(&u(r"\\?\z:\a\b"), answer(3));
        windows_verdict(&u(r"\\?\Volume{ab-12}\a"), answer(3));
        assert_eq!(*asked.borrow(), vec![r"Z:\", r"\\?\Volume{ab-12}\"]);
        // A share is remote without asking, and a path of no known shape is
        // not assumed to be local.
        let never = |_: &[u16]| -> u32 { panic!("not asked") };
        for share in [r"\\srv\s\x", r"\\?\UNC\srv\s", "//srv/s"] {
            assert_eq!(windows_verdict(&u(share), never), Verdict::Remote);
        }
        for odd in [r"\\.\C:", "", "nonsense", r"\\?\GLOBALROOT\x"] {
            assert_eq!(windows_verdict(&u(odd), never), Verdict::Unknown, "{odd}");
        }
    }
}
