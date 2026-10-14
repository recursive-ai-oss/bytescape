//! Drive enumeration and capacity numbers.
//!
//! [`list_drives`] answers "which drives are there, which of them should be
//! scanned in the background, and how full is each". [`volume_for_path`]
//! answers "how big is the volume that holds this folder", which the scanner
//! uses to turn counted bytes into a real percentage.
//!
//! Everything returned is plain data. Nothing here opens a file inside a
//! drive; only mount tables, device properties and free-space counters are
//! read.
//!
//! # What counts as one drive
//!
//! On Linux a drive is one filesystem, not one mount. Mounts are merged when
//! they belong to the same filesystem:
//!
//! - mounts whose source is a device node (`/dev/...`) are the same drive when
//!   the source is the same. This merges bind mounts and the subvolumes of one
//!   btrfs filesystem, which the kernel reports with different device numbers;
//! - mounts of a ZFS pool are the same drive when the pool name is the same;
//! - network mounts are the same drive when the file system type and the
//!   remote source are the same;
//! - any other mount is its own drive.
//!
//! The mount that represents a merged drive is the one whose root is the top of
//! the filesystem, then the one nearest to `/`, then the oldest. When two
//! mounts cover the same directory only the later one is visible and the
//! earlier one is ignored.
//!
//! Mounts of pseudo filesystems (`proc`, `tmpfs`, `overlay`, `squashfs` snap
//! images and so on), the EFI system partition and desktop portals are not
//! listed at all. Only a short list of known disk, optical and network
//! filesystems is listed; an unknown type is treated as a pseudo filesystem.
//! [`volume_for_path`] still answers for a folder on such a mount.
//!
//! On Windows a drive is a drive letter. Volumes that are mounted on a folder
//! instead of a letter are not listed, but [`volume_for_path`] resolves them.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

mod capacity;
mod probe;

#[cfg(any(target_os = "linux", test))]
mod mountinfo;
#[cfg(any(target_os = "linux", test))]
mod rules;
#[cfg(target_os = "linux")]
mod sysfs;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;
#[cfg(any(windows, test))]
mod winkind;

pub use capacity::{Capacity, ReservedKind};

/// How long one drive may take to report its size before it is shown without
/// one. A dead network mount can block for minutes inside the operating
/// system, so every size query runs on its own thread and is abandoned after
/// this long.
pub const CAPACITY_TIMEOUT: Duration = Duration::from_millis(2500);

/// A stable identifier for a drive.
///
/// The text is opaque. It is the same for the same filesystem from one run to
/// the next whenever the system gives the filesystem a lasting identity: the
/// filesystem UUID on Linux and the volume GUID on Windows. Without one it
/// falls back to the device node or the mount point.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DriveId(String);

impl DriveId {
    /// Wraps an identifier produced by this module or stored earlier.
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    /// The identifier text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DriveId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What sort of drive this is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum DriveKind {
    /// A disk built into the machine.
    Fixed,
    /// Removable storage: USB disks and sticks, memory cards, optical discs.
    Removable,
    /// A network share.
    Network,
    /// Anything else that holds files, such as a user-space filesystem or an
    /// attached disk image.
    Other,
}

/// Why a drive has no size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CapacityProblem {
    /// The operating system did not answer in time.
    TimedOut,
    /// The query failed.
    Failed(io::ErrorKind),
    /// A per-user disk quota is in force and this system cannot report the
    /// real size of the volume, so no honest used figure exists.
    QuotaHidesVolumeSize,
}

/// One mount of a drive.
///
/// A scan of a drive starts at one of its mounts and may meet others below it.
/// A mount of the same drive is the same filesystem, so the scan crosses into
/// it, except when that would count the same files twice: a bind mount of a
/// folder that the scan already reaches through the mount it started from.
/// Decide that by comparing roots: the mount below the start is skipped when
/// its [`root`](Mount::root) lies inside the start mount's root
/// ([`Mount::root_is_within`]). [`covered_by_other_mount`](Mount::covered_by_other_mount)
/// is the same test made against every other mount of the drive, for a scan
/// that starts at the drive's own mount point.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mount {
    /// Where the mount is attached.
    pub mount_point: PathBuf,
    /// The kernel's mount ID (the first field of `/proc/self/mountinfo`). It
    /// equals the `stx_mnt_id` that `statx` returns for `STATX_MNT_ID`, not the
    /// 64-bit `STATX_MNT_ID_UNIQUE` value. `None` on Windows.
    pub mount_id: Option<u32>,
    /// The folder of the filesystem that this mount shows, as a path inside
    /// the filesystem: `/` for the whole filesystem, `/@home` for a btrfs
    /// subvolume, `/alice/projects` for a bind mount of a folder. `\` on
    /// Windows.
    pub root: PathBuf,
    /// Another mount of this drive already shows this mount's folder (this
    /// root lies inside that one's root, or is the same and that mount comes
    /// first in the list). Following this mount would count its files twice.
    pub covered_by_other_mount: bool,
}

impl Mount {
    /// Whether this mount's root lies inside `other`'s root. Roots are compared
    /// by whole path components.
    pub fn root_is_within(&self, other: &Mount) -> bool {
        self.root.starts_with(&other.root)
    }
}

/// One drive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Drive {
    /// Stable identifier.
    pub id: DriveId,
    /// The root folder to scan: `/` or `/mnt/data` on Linux, `C:\` on Windows.
    pub mount_point: PathBuf,
    /// The volume label, when it has one and the system can tell.
    pub label: Option<String>,
    /// The filesystem type as the system names it, for example `ext4` or
    /// `NTFS`. Empty when the system did not say.
    pub fs_type: String,
    /// The kind of drive.
    pub kind: DriveKind,
    /// Whether the drive is mounted read-only.
    pub read_only: bool,
    /// Whether the drive should be scanned in the background at launch: a
    /// fixed drive whose size is known.
    pub eligible_for_background_scan: bool,
    /// The sizes, when the system reported them.
    pub capacity: Option<Capacity>,
    /// Why `capacity` is missing.
    pub capacity_problem: Option<CapacityProblem>,
    /// Every mount of this drive, the one that stands for it first. A
    /// Windows drive has one. The walker uses the list to cross into
    /// the same drive's mounts below its root; see [`Mount`].
    pub mounts: Vec<Mount>,
}

/// Reasons a drive question could not be answered.
#[derive(Debug)]
#[non_exhaustive]
pub enum DriveError {
    /// The list of mounts or drives could not be read.
    MountTable(io::Error),
    /// The path could not be examined.
    Path(io::Error),
    /// The system did not answer in time (a dead network mount, for example).
    TimedOut,
    /// No drive holds this path.
    NoVolume,
    /// This operating system is not supported.
    Unsupported,
}

impl fmt::Display for DriveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MountTable(error) => write!(f, "cannot read the list of drives: {error}"),
            Self::Path(error) => write!(f, "cannot examine the path: {error}"),
            Self::TimedOut => f.write_str("the system did not answer in time"),
            Self::NoVolume => f.write_str("no drive holds this path"),
            Self::Unsupported => f.write_str("drives are not supported on this system"),
        }
    }
}

impl std::error::Error for DriveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::MountTable(error) | Self::Path(error) => Some(error),
            _ => None,
        }
    }
}

/// Whether a drive is scanned in the background at launch.
///
/// Only fixed drives with a known size qualify. Removable drives, network
/// shares and everything else wait until the user picks them.
pub(crate) fn eligible_for_background_scan(kind: DriveKind, capacity: Option<&Capacity>) -> bool {
    kind == DriveKind::Fixed && capacity.is_some_and(|c| c.total > 0)
}

/// Orders drives for display: fixed first, then removable, network and other,
/// each group by mount point.
pub(crate) fn sort_for_display(drives: &mut [Drive]) {
    drives.sort_by(|a, b| {
        (a.kind, &a.mount_point, a.id.as_str()).cmp(&(b.kind, &b.mount_point, b.id.as_str()))
    });
}

/// Lists the drives of this machine, fixed drives first.
///
/// A drive that does not report its size in [`CAPACITY_TIMEOUT`] is still
/// listed, without a size and not eligible for background scanning.
pub fn list_drives() -> Result<Vec<Drive>, DriveError> {
    #[cfg(target_os = "linux")]
    {
        linux::Host::system().list()
    }
    #[cfg(windows)]
    {
        Ok(windows::list(CAPACITY_TIMEOUT))
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        Err(DriveError::Unsupported)
    }
}

/// Finds the drive that holds `path` and its sizes.
///
/// The path does not have to be a mount point, and it may be on a mount that
/// [`list_drives`] hides. Looking the path up can touch a slow or dead network
/// mount, so it runs on a worker thread and gives up with
/// [`DriveError::TimedOut`] after [`CAPACITY_TIMEOUT`]; the call as a whole
/// takes at most twice that. Symbolic links in the path are followed, because the
/// caller named a place; the scanner never follows them below that place.
/// The sizes are the operating system's own used-bytes figure, which is what
/// scan progress is measured against.
pub fn volume_for_path(path: &Path) -> Result<Drive, DriveError> {
    #[cfg(target_os = "linux")]
    {
        linux::Host::system().volume_for_path(path)
    }
    #[cfg(windows)]
    {
        windows::volume_for_path(path, CAPACITY_TIMEOUT)
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = path;
        Err(DriveError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(total: u64) -> Capacity {
        Capacity {
            total,
            used: 0,
            free: total,
            reserved: 0,
            reserved_kind: ReservedKind::FilesystemReserve,
        }
    }

    #[test]
    fn background_scan_eligibility_table() {
        let some = cap(1000);
        let zero = cap(0);
        let cases: [(DriveKind, Option<&Capacity>, bool); 9] = [
            (DriveKind::Fixed, Some(&some), true),
            (DriveKind::Fixed, Some(&zero), false),
            (DriveKind::Fixed, None, false),
            (DriveKind::Removable, Some(&some), false),
            (DriveKind::Removable, None, false),
            (DriveKind::Network, Some(&some), false),
            (DriveKind::Network, None, false),
            (DriveKind::Other, Some(&some), false),
            (DriveKind::Other, None, false),
        ];
        for (kind, capacity, expected) in cases {
            assert_eq!(
                eligible_for_background_scan(kind, capacity),
                expected,
                "{kind:?} {capacity:?}"
            );
        }
    }

    fn drive(kind: DriveKind, mount: &str) -> Drive {
        Drive {
            id: DriveId::new(mount),
            mount_point: PathBuf::from(mount),
            label: None,
            fs_type: String::new(),
            kind,
            read_only: false,
            eligible_for_background_scan: false,
            capacity: None,
            capacity_problem: None,
            mounts: Vec::new(),
        }
    }

    #[test]
    fn display_order_is_fixed_first_then_by_mount_point() {
        let mut drives = vec![
            drive(DriveKind::Network, "/mnt/nas"),
            drive(DriveKind::Fixed, "/home"),
            drive(DriveKind::Removable, "/media/usb"),
            drive(DriveKind::Fixed, "/"),
            drive(DriveKind::Other, "/mnt/fuse"),
        ];
        sort_for_display(&mut drives);
        let order: Vec<_> = drives.iter().map(|d| d.mount_point.clone()).collect();
        let expected: Vec<PathBuf> = ["/", "/home", "/media/usb", "/mnt/nas", "/mnt/fuse"]
            .iter()
            .map(PathBuf::from)
            .collect();
        assert_eq!(order, expected);
    }

    fn mount(root: &str) -> Mount {
        Mount {
            mount_point: PathBuf::from("/m"),
            mount_id: Some(1),
            root: PathBuf::from(root),
            covered_by_other_mount: false,
        }
    }

    #[test]
    fn roots_are_compared_by_whole_components() {
        assert!(mount("/alice/projects").root_is_within(&mount("/")));
        assert!(mount("/alice/projects").root_is_within(&mount("/alice")));
        assert!(mount("/alice").root_is_within(&mount("/alice")));
        assert!(!mount("/alice").root_is_within(&mount("/alice/projects")));
        assert!(!mount("/alicia").root_is_within(&mount("/alice")));
        // Sibling btrfs subvolumes do not contain one another.
        assert!(!mount("/@home").root_is_within(&mount("/@")));
    }

    #[test]
    fn drive_id_round_trips_text() {
        let id = DriveId::new("uuid:1234");
        assert_eq!(id.as_str(), "uuid:1234");
        assert_eq!(id.to_string(), "uuid:1234");
    }
}
