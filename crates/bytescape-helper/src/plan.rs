//! Choosing the reader for a scan, and running it with the walker behind it.
//!
//! The walker reads any filesystem. The raw readers (ext4, btrfs, XFS on
//! Linux; NTFS on Windows) are faster and see more, but each is used only for
//! the one kind of filesystem it reads and only where it reads it correctly:
//!
//! - a raw ext4 or XFS read shows the whole filesystem, so it is used only for
//!   a path that is exactly where one is mounted;
//! - the btrfs reader starts at a directory of a mounted filesystem and covers
//!   its nested subvolumes, so a mount point of a subvolume is enough;
//! - the NTFS reader always covers a whole volume, so it is used only for the
//!   root of a fixed NTFS drive that the helper found itself.
//!
//! The filesystem type comes from the helper's own view of the mount table (or
//! the drive list), never from the request. A reader that refuses before it
//! has sent anything leaves the walker to do the scan, and the reason goes back
//! in the answer.

use bs_ipc::{Backend, Fallback};
use bs_scan::{ScanContext, ScanEnd as EngineEnd, ScanError, Scanner, Walker};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The reader a plan runs first.
pub(crate) enum Reader {
    /// The walker only.
    Walker,
    /// The raw ext4 reader on this block device.
    #[cfg(target_os = "linux")]
    Ext4 {
        device: PathBuf,
        options: bs_scan_ext4::Ext4Options,
    },
    /// The btrfs tree search.
    #[cfg(target_os = "linux")]
    Btrfs(bs_scan_btrfs::BtrfsOptions),
    /// The XFS bulk-stat reader.
    #[cfg(target_os = "linux")]
    Xfs(bs_scan_xfs::XfsOptions),
    /// The NTFS file table reader on this volume. Both paths are built by
    /// [`ntfs_target`] from the drive list and from nothing else.
    #[cfg(any(windows, test))]
    Ntfs {
        device: PathBuf,
        root: PathBuf,
        options: bs_scan_ntfs::NtfsOptions,
    },
    /// A scanner made by a test.
    #[cfg(test)]
    Stub(Box<dyn Scanner>),
}

/// What reader runs, and why.
pub(crate) struct Plan {
    pub backend: Backend,
    pub fallback: Fallback,
    pub reader: Reader,
    /// The mounts of the same filesystem that the scan may enter (Linux):
    /// the ones the request names that really are mounts of the scan root's
    /// filesystem.
    pub mount_ids: Vec<u64>,
}

impl Plan {
    fn walker(fallback: Fallback) -> Plan {
        Plan {
            backend: Backend::Walker,
            fallback,
            reader: Reader::Walker,
            mount_ids: Vec::new(),
        }
    }
}

/// The scanner the helper runs: the plan's reader, with the walker behind it
/// for a filesystem the reader refuses.
pub(crate) struct Chosen {
    pub plan: Plan,
    /// What actually ran.
    pub ran: Mutex<(Backend, Fallback)>,
}

impl Chosen {
    pub(crate) fn new(plan: Plan) -> Chosen {
        Chosen {
            ran: Mutex::new((plan.backend, plan.fallback)),
            plan,
        }
    }

    /// What ran, and why it was not the reader asked for.
    pub(crate) fn outcome(&self) -> (Backend, Fallback) {
        self.ran
            .lock()
            .map(|ran| *ran)
            .unwrap_or((Backend::Walker, Fallback::None))
    }

    fn note(&self, backend: Backend, fallback: Fallback) {
        if let Ok(mut ran) = self.ran.lock() {
            *ran = (backend, fallback);
        }
    }
}

impl Scanner for Chosen {
    fn name(&self) -> &'static str {
        "helper"
    }

    fn scan(&self, root: &Path, ctx: &ScanContext) -> Result<EngineEnd, ScanError> {
        self.note(self.plan.backend, self.plan.fallback);
        let first = match &self.plan.reader {
            Reader::Walker => None,
            #[cfg(target_os = "linux")]
            Reader::Ext4 { device, options } => Some(
                bs_scan_ext4::Ext4Scanner::new(device.clone(), options.clone()).scan(root, ctx),
            ),
            #[cfg(target_os = "linux")]
            Reader::Btrfs(options) => {
                Some(bs_scan_btrfs::BtrfsScanner::new(options.clone()).scan(root, ctx))
            }
            #[cfg(target_os = "linux")]
            Reader::Xfs(options) => {
                Some(bs_scan_xfs::XfsScanner::new(options.clone()).scan(root, ctx))
            }
            #[cfg(any(windows, test))]
            Reader::Ntfs {
                device,
                root: volume_root,
                options,
            } => Some(
                bs_scan_ntfs::NtfsScanner::new(device.clone(), options.clone())
                    .scan(volume_root, ctx),
            ),
            #[cfg(test)]
            Reader::Stub(scanner) => Some(scanner.scan(root, ctx)),
        };
        match first {
            None => Walker.scan(root, ctx),
            Some(Ok(end)) => Ok(end),
            Some(Err(error)) => {
                // The scan root is not the folder that was judged. The walker
                // is held to the same pin and would refuse too, but a scan
                // that found the ground moved is not retried by another reader.
                if matches!(error, ScanError::RootMoved { .. }) {
                    return Err(error);
                }
                // A raw reader only fails before it has sent anything. If
                // something was sent the walker must not repeat it.
                if ctx.progress().batches > 0 {
                    return Err(error);
                }
                self.note(Backend::Walker, fallback_for(&error));
                Walker.scan(root, ctx)
            }
        }
    }
}

/// What the interface is told about a reader that refused.
pub(crate) fn fallback_for(error: &ScanError) -> Fallback {
    let ScanError::Root(io) = error else {
        return Fallback::NotSupported;
    };
    let Some(inner) = io.get_ref() else {
        return Fallback::NotSupported;
    };
    #[cfg(target_os = "linux")]
    {
        use bs_scan_btrfs::BtrfsError;
        use bs_scan_ext4::Ext4Error;
        use bs_scan_xfs::XfsError;
        if let Some(e) = inner.downcast_ref::<Ext4Error>() {
            return match e {
                Ext4Error::TooLarge(_) => Fallback::TooLarge,
                Ext4Error::PartialMount => Fallback::PartialMount,
                _ => Fallback::NotSupported,
            };
        }
        if let Some(e) = inner.downcast_ref::<BtrfsError>() {
            return match e {
                BtrfsError::TooLarge(_) => Fallback::TooLarge,
                BtrfsError::MountNotPlaced(_) => Fallback::MountNotPlaced,
                _ => Fallback::NotSupported,
            };
        }
        if let Some(e) = inner.downcast_ref::<XfsError>() {
            return match e {
                XfsError::TooLarge(_) => Fallback::TooLarge,
                _ => Fallback::NotSupported,
            };
        }
    }
    #[cfg(any(windows, test))]
    if let Some(e) = inner.downcast_ref::<bs_scan_ntfs::NtfsError>() {
        return match e {
            bs_scan_ntfs::NtfsError::TooLarge(_) => Fallback::TooLarge,
            _ => Fallback::NotSupported,
        };
    }
    Fallback::NotSupported
}

// ------------------------------------------------------------------ Linux

/// Decides which reader runs for `requested` on `root`, given the mount table.
/// `device_ok` says whether a device named by the table, which the table gives
/// the device number of, may be read raw. `requested_mounts` are the mount IDs
/// the request names as further mounts of the drive; only those that are mounts
/// of the same filesystem as `root`'s are kept.
///
/// `root_mount_id` is the mount ID that `statx` reports for `root` itself. The
/// mount table is searched by path, and a path can be hidden: a mount placed
/// over a shorter path covers the mounts below it, but the table still lists
/// them. The table's mount is used only when it is the one `statx` says `root`
/// is on; otherwise (or when the kernel gives no mount ID) the table does not
/// describe what is there, so the walker reads it and no other mount is
/// entered.
#[cfg(target_os = "linux")]
pub(crate) fn choose(
    requested: Backend,
    root: &Path,
    mounts: &[crate::mounts::Mount],
    device_ok: &dyn Fn(&Path, (u32, u32)) -> bool,
    requested_mounts: &[u64],
    root_mount_id: Option<u64>,
) -> Plan {
    use crate::mounts::{already_shown_ids, at_exactly, containing, same_filesystem};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let path = root.as_os_str().as_bytes();
    let in_view = |m: &&crate::mounts::Mount| root_mount_id == Some(m.id);
    // Only mounts of the filesystem the scan is on may be entered, whatever
    // the request says: crossing into another filesystem is never allowed.
    // Nor is a mount that would show files the scan counts anyway: one that
    // shows the scan's own start again, or one that another mount the scan
    // reaches already shows (a bind mount of a folder inside it). That is
    // judged from where this scan starts. The list comes from the interface,
    // which is not trusted to have got either right.
    let mount_ids: Vec<u64> = containing(mounts, path)
        .filter(in_view)
        .map(|home| {
            let dropped: std::collections::HashSet<u64> =
                already_shown_ids(mounts, home, path, requested_mounts)
                    .into_iter()
                    .collect();
            requested_mounts
                .iter()
                .copied()
                .filter(|id| {
                    !dropped.contains(id)
                        && mounts
                            .iter()
                            .any(|m| m.id == *id && m.id != home.id && same_filesystem(m, home))
                })
                .collect()
        })
        .unwrap_or_default();
    let walker = |fallback| Plan {
        mount_ids: mount_ids.clone(),
        ..Plan::walker(fallback)
    };
    // A reader that was asked for by name and is not used says so; the
    // automatic choice has nothing to explain.
    let unsuitable = if requested == Backend::Auto {
        Fallback::None
    } else {
        Fallback::NotSupported
    };
    let wanted = match requested {
        Backend::Walker => return walker(Fallback::None),
        Backend::Auto => None,
        Backend::Ext4 => Some(Kind::Ext4),
        Backend::Btrfs => Some(Kind::Btrfs),
        Backend::Xfs => Some(Kind::Xfs),
        // The NTFS reader exists only on Windows.
        _ => return walker(Fallback::Unavailable),
    };
    // Each raw reader reads from where a filesystem is mounted.
    let Some(mount) = at_exactly(mounts, path).filter(in_view) else {
        return walker(unsuitable);
    };
    let Some(kind) = Kind::of(&mount.fs_type) else {
        return walker(unsuitable);
    };
    if wanted.is_some_and(|w| w != kind) {
        return walker(unsuitable);
    }
    let reader = match kind {
        Kind::Ext4 => {
            let device = PathBuf::from(std::ffi::OsString::from_vec(mount.source.clone()));
            if !device_ok(&device, mount.device) {
                return walker(unsuitable);
            }
            let others = mounts
                .iter()
                .filter(|m| m.id != mount.id)
                .map(|m| PathBuf::from(std::ffi::OsStr::from_bytes(&m.mount_point)));
            let options = bs_scan_ext4::Ext4Options {
                // The mount table's own word for what this mount shows; a
                // mount of part of a filesystem is refused by the reader.
                mount_root: Some(mount.root.clone()),
                ..bs_scan_ext4::Ext4Options::default()
            }
            .with_other_mounts(root, others);
            Reader::Ext4 { device, options }
        }
        Kind::Btrfs => Reader::Btrfs(bs_scan_btrfs::BtrfsOptions::default()),
        Kind::Xfs => {
            // The bulk-stat reader reads every inode of the filesystem; for a
            // mount of part of it that is the wrong amount of work, and the
            // walker is the honest reader.
            if mount.root != b"/" {
                return walker(unsuitable);
            }
            Reader::Xfs(bs_scan_xfs::XfsOptions::default())
        }
    };
    Plan {
        backend: kind.backend(),
        fallback: Fallback::None,
        reader,
        mount_ids,
    }
}

/// A filesystem that has a raw reader.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Ext4,
    Btrfs,
    Xfs,
}

#[cfg(target_os = "linux")]
impl Kind {
    fn of(fs_type: &str) -> Option<Kind> {
        match fs_type {
            "ext4" | "ext3" | "ext2" => Some(Kind::Ext4),
            "btrfs" => Some(Kind::Btrfs),
            "xfs" => Some(Kind::Xfs),
            _ => None,
        }
    }

    fn backend(self) -> Backend {
        match self {
            Kind::Ext4 => Backend::Ext4,
            Kind::Btrfs => Backend::Btrfs,
            Kind::Xfs => Backend::Xfs,
        }
    }
}

/// The device is read as the superuser, so it must be a block device under
/// `/dev` that the mount table names, and its device number must be the one
/// the table shows for the mount. A name in the table that leads to another
/// device than the mounted one (a replaced node, a link) is not read.
#[cfg(target_os = "linux")]
pub(crate) fn is_block_device_under_dev(device: &Path, expected: (u32, u32)) -> bool {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    device.starts_with("/dev")
        && std::fs::canonicalize(device)
            .ok()
            .filter(|real| real.starts_with("/dev"))
            .and_then(|real| std::fs::metadata(real).ok())
            .is_some_and(|meta| {
                meta.file_type().is_block_device() && is_device_number(meta.rdev(), expected)
            })
}

/// Whether the device number `rdev` is `major:minor`.
#[cfg(target_os = "linux")]
fn is_device_number(rdev: u64, expected: (u32, u32)) -> bool {
    (libc::major(rdev), libc::minor(rdev)) == expected
}

/// Opens the folder `root` of a scan, to ask about it through the descriptor.
///
/// `O_DIRECTORY` makes the open fail on anything that is not a folder, so a
/// name swapped for a pipe or a device is refused rather than opened (opening
/// one can block, or act on a device). `O_NOFOLLOW` covers the last name.
#[cfg(target_os = "linux")]
pub(crate) fn open_scan_root(root: &Path) -> Option<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root)
        .ok()
}

/// The mount ID that `statx` reports for an open folder, if the kernel reports
/// one (Linux 5.8 and later). Asked of the descriptor, never of a name.
#[cfg(target_os = "linux")]
pub(crate) fn mount_id_of_fd(folder: std::os::fd::BorrowedFd<'_>) -> Option<u64> {
    use std::os::fd::AsRawFd;
    bs_fsops::remove::fstat(folder.as_raw_fd()).ok()?.mnt
}

/// The plan for a scan on this computer. `root_mount_id` is the mount ID of
/// the judged root, taken from the descriptor the helper holds for it: the
/// plan does not open the root again.
#[cfg(target_os = "linux")]
pub(crate) fn plan_for(
    requested: Backend,
    root: &Path,
    host: &crate::scan::Host,
    requested_mounts: &[u64],
    root_mount_id: Option<u64>,
) -> Plan {
    let mounts = std::fs::read(&host.mountinfo)
        .map(|text| crate::mounts::parse(&text))
        .unwrap_or_default();
    choose(
        requested,
        root,
        &mounts,
        &is_block_device_under_dev,
        requested_mounts,
        root_mount_id,
    )
}

// ---------------------------------------------------------------- Windows

/// The volume the NTFS reader opens, and the root it is told it is reading.
#[cfg(any(windows, test))]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct NtfsTarget {
    /// `\\.\X:`
    pub device: PathBuf,
    /// `X:\`
    pub root: PathBuf,
}

/// The NTFS volume for a scan of `canonical_root`, if it is the root of one of
/// `drives`.
///
/// `drives` are the letters of the fixed NTFS drives that the helper found
/// itself. The device name is built from one of those letters and nothing the
/// request said: whatever else the request's path looks like (a device name, a
/// UNC share, an image file, a folder), it never becomes the path of a raw
/// volume.
#[cfg(any(windows, test))]
pub(crate) fn ntfs_target(canonical_root: &str, drives: &[char]) -> Option<NtfsTarget> {
    let plain = bs_fsops::winpath::strip_extended_prefix(canonical_root);
    let mut chars = plain.chars();
    let (letter, colon, slash) = (chars.next()?, chars.next()?, chars.next()?);
    if chars.next().is_some() || colon != ':' || slash != '\\' {
        return None;
    }
    let letter = letter.to_ascii_uppercase();
    let letter = drives.iter().copied().find(|d| *d == letter)?;
    Some(NtfsTarget {
        device: PathBuf::from(format!(r"\\.\{letter}:")),
        root: PathBuf::from(format!(r"{letter}:\")),
    })
}

/// Decides which reader runs for `requested` on `canonical_root` (Windows).
///
/// `drives` are the fixed NTFS drive letters the helper enumerated;
/// `available_memory` is the physical memory the system reports as free.
#[cfg(any(windows, test))]
pub(crate) fn choose_windows(
    requested: Backend,
    canonical_root: &str,
    drives: &[char],
    available_memory: Option<u64>,
) -> Plan {
    let unsuitable = if requested == Backend::Auto {
        Fallback::None
    } else {
        Fallback::NotSupported
    };
    match requested {
        Backend::Walker => Plan::walker(Fallback::None),
        Backend::Auto | Backend::Ntfs => match ntfs_target(canonical_root, drives) {
            Some(target) => Plan {
                backend: Backend::Ntfs,
                fallback: Fallback::None,
                reader: Reader::Ntfs {
                    device: target.device,
                    root: target.root,
                    options: bs_scan_ntfs::NtfsOptions::default()
                        .with_available_memory(available_memory),
                },
                mount_ids: Vec::new(),
            },
            None => Plan::walker(unsuitable),
        },
        // The Linux readers do not exist here.
        _ => Plan::walker(Fallback::Unavailable),
    }
}

/// The plan for a scan on this computer.
#[cfg(windows)]
pub(crate) fn plan_for(
    requested: Backend,
    root: &Path,
    _host: &crate::scan::Host,
    _requested_mounts: &[u64],
    _root_mount_id: Option<u64>,
) -> Plan {
    let drives = crate::windows::fixed_ntfs_drives();
    choose_windows(
        requested,
        &root.to_string_lossy(),
        &drives,
        crate::windows::available_memory(),
    )
}

#[cfg(not(any(target_os = "linux", windows)))]
pub(crate) fn plan_for(
    requested: Backend,
    _root: &Path,
    _host: &crate::scan::Host,
    _requested_mounts: &[u64],
    _root_mount_id: Option<u64>,
) -> Plan {
    Plan::walker(match requested {
        Backend::Auto | Backend::Walker => Fallback::None,
        _ => Fallback::Unavailable,
    })
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects
    )]
    // The tests build a small ext4 image with the system's tool. The helper
    // itself starts nothing.
    #![allow(clippy::disallowed_types, clippy::disallowed_methods)]
    use super::*;
    use bs_scan::ScanOptions;
    use bs_tree::Aggregator;

    fn error_of<E: std::error::Error + Send + Sync + 'static>(e: E) -> ScanError {
        ScanError::Root(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    #[test]
    fn what_a_reader_reports_maps_to_what_the_interface_is_told() {
        #[cfg(target_os = "linux")]
        {
            use bs_scan_btrfs::BtrfsError;
            use bs_scan_ext4::Ext4Error;
            use bs_scan_xfs::XfsError;
            for (error, want) in [
                (error_of(Ext4Error::TooLarge("x")), Fallback::TooLarge),
                (error_of(Ext4Error::PartialMount), Fallback::PartialMount),
                (error_of(Ext4Error::NotExt4), Fallback::NotSupported),
                (error_of(BtrfsError::TooLarge("x")), Fallback::TooLarge),
                (
                    error_of(BtrfsError::MountNotPlaced("x")),
                    Fallback::MountNotPlaced,
                ),
                (error_of(BtrfsError::NotBtrfs), Fallback::NotSupported),
                (error_of(BtrfsError::Hostile("x")), Fallback::NotSupported),
                (error_of(XfsError::TooLarge("x")), Fallback::TooLarge),
                (error_of(XfsError::NotXfs), Fallback::NotSupported),
                (error_of(XfsError::BadRoot("x")), Fallback::NotSupported),
            ] {
                assert_eq!(fallback_for(&error), want, "{error:?}");
            }
        }
        use bs_scan_ntfs::NtfsError;
        for (error, want) in [
            (error_of(NtfsError::TooLarge("x")), Fallback::TooLarge),
            (error_of(NtfsError::NotNtfs), Fallback::NotSupported),
            (error_of(NtfsError::BadMft("x")), Fallback::NotSupported),
        ] {
            assert_eq!(fallback_for(&error), want, "{error:?}");
        }
        // An error that is only an I/O error (the device would not open, the
        // ioctl was refused), and one that is not a root error at all.
        assert_eq!(
            fallback_for(&ScanError::Root(std::io::Error::other("x"))),
            Fallback::NotSupported
        );
        assert_eq!(
            fallback_for(&ScanError::Unsupported),
            Fallback::NotSupported
        );
    }

    // ------------------------------------------------- Running the readers

    /// A reader that fails the way the real ones do, to try the hand-over to
    /// the walker without a filesystem of every kind.
    struct Failing {
        error: fn() -> ScanError,
        /// Send a batch before failing, as a reader that fails half way does.
        after_batch: bool,
    }

    impl Scanner for Failing {
        fn name(&self) -> &'static str {
            "failing"
        }

        fn scan(&self, _root: &Path, ctx: &ScanContext) -> Result<EngineEnd, ScanError> {
            if self.after_batch {
                let mut batch = bs_tree::Batch::new();
                let meta = bs_tree::Meta {
                    logical: 0,
                    allocated: 0,
                    mtime: 0,
                    exclusive: 0,
                    flags: bs_tree::NodeFlags::default(),
                    category: bs_tree::Category::OTHER,
                    link: None,
                    failure: None,
                };
                batch
                    .dir(bs_tree::DirKey(0), bs_tree::DirKey::NONE, b"root", &meta)
                    .unwrap();
                batch.listing_done(bs_tree::DirKey(0));
                assert!(ctx.send(batch));
            }
            Err((self.error)())
        }
    }

    fn run_chosen(plan: Plan, root: &Path) -> (Chosen, Aggregator, Result<EngineEnd, ScanError>) {
        let chosen = Chosen::new(plan);
        let (ctx, batches) = ScanContext::channel(ScanOptions::default(), 1024);
        let end = chosen.scan(root, &ctx);
        let mut aggregator = Aggregator::new();
        while let Ok(batch) = batches.try_recv() {
            aggregator.apply(&batch);
        }
        aggregator.finish();
        (chosen, aggregator, end)
    }

    fn stub_plan(backend: Backend, error: fn() -> ScanError, after_batch: bool) -> Plan {
        Plan {
            backend,
            fallback: Fallback::None,
            reader: Reader::Stub(Box::new(Failing { error, after_batch })),
            mount_ids: Vec::new(),
        }
    }

    /// A reader asked for, how it fails, and what the interface is told.
    type Case = (Backend, fn() -> ScanError, Fallback);

    /// A folder made for one test with two files in a tree, removed when it
    /// is dropped. Nothing outside it is touched.
    struct Folder(PathBuf);

    impl Folder {
        fn p(&self, relative: &str) -> PathBuf {
            self.0.join(relative)
        }
    }

    impl Drop for Folder {
        fn drop(&mut self) {
            let ours = self
                .0
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("bs-helper-plan-"));
            if ours {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    fn folder_with_files() -> Folder {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "bs-helper-plan-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("tree/d")).unwrap();
        std::fs::write(root.join("tree/a"), "1").unwrap();
        std::fs::write(root.join("tree/d/b"), "22").unwrap();
        Folder(root)
    }

    fn files_below_root(aggregator: &mut Aggregator) -> u64 {
        aggregator
            .publish()
            .root()
            .map_or(0, |r| u64::from(r.file_count()))
    }

    #[test]
    fn a_reader_that_refuses_before_sending_leaves_the_walker_to_do_the_scan_and_says_why() {
        let fx = folder_with_files();
        #[cfg(target_os = "linux")]
        let cases: Vec<Case> = vec![
            (
                Backend::Btrfs,
                || error_of(bs_scan_btrfs::BtrfsError::MountNotPlaced("sibling")),
                Fallback::MountNotPlaced,
            ),
            (
                Backend::Btrfs,
                || error_of(bs_scan_btrfs::BtrfsError::TooLarge("memory")),
                Fallback::TooLarge,
            ),
            (
                Backend::Btrfs,
                || ScanError::Root(std::io::Error::from_raw_os_error(1)),
                Fallback::NotSupported,
            ),
            (
                Backend::Xfs,
                || error_of(bs_scan_xfs::XfsError::TooLarge("memory")),
                Fallback::TooLarge,
            ),
            (
                Backend::Xfs,
                || error_of(bs_scan_xfs::XfsError::Hostile("reply")),
                Fallback::NotSupported,
            ),
        ];
        #[cfg(not(target_os = "linux"))]
        let cases: Vec<Case> = Vec::new();
        let ntfs: Vec<Case> = vec![
            (
                Backend::Ntfs,
                || error_of(bs_scan_ntfs::NtfsError::TooLarge("memory")),
                Fallback::TooLarge,
            ),
            (
                Backend::Ntfs,
                || error_of(bs_scan_ntfs::NtfsError::NotNtfs),
                Fallback::NotSupported,
            ),
        ];
        for (backend, error, want) in cases.into_iter().chain(ntfs) {
            let (chosen, mut tree, end) =
                run_chosen(stub_plan(backend, error, false), &fx.p("tree"));
            assert_eq!(end.unwrap(), EngineEnd::Completed);
            assert_eq!(chosen.outcome(), (Backend::Walker, want), "{backend:?}");
            assert_eq!(files_below_root(&mut tree), 2);
        }
    }

    #[test]
    fn a_reader_that_fails_after_sending_is_not_repeated_by_the_walker() {
        let fx = folder_with_files();
        let (chosen, _tree, end) = run_chosen(
            stub_plan(
                Backend::Ntfs,
                || error_of(bs_scan_ntfs::NtfsError::BadMft("half way")),
                true,
            ),
            &fx.p("tree"),
        );
        assert!(end.is_err(), "the error reaches the caller");
        // What ran is still the reader: the walker did not start over.
        assert_eq!(chosen.outcome(), (Backend::Ntfs, Fallback::None));
    }

    #[test]
    fn a_root_that_moved_is_not_handed_to_another_reader() {
        let fx = folder_with_files();
        // Whichever reader found the root moved, the walker does not take over:
        // the scan ends with the error, nothing is read, and what ran is still
        // the reader that refused.
        for backend in [Backend::Ntfs, Backend::Btrfs, Backend::Xfs, Backend::Ext4] {
            let (chosen, mut tree, end) = run_chosen(
                stub_plan(
                    backend,
                    || ScanError::RootMoved {
                        pinned: bs_scan::RootKey::Mount {
                            id: 7,
                            unique: true,
                        },
                        found: None,
                    },
                    false,
                ),
                &fx.p("tree"),
            );
            assert!(
                matches!(end, Err(ScanError::RootMoved { .. })),
                "{backend:?}: {end:?}"
            );
            assert_eq!(files_below_root(&mut tree), 0, "{backend:?}");
            assert_eq!(chosen.outcome(), (backend, Fallback::None), "{backend:?}");
        }
    }

    #[test]
    fn a_reader_that_works_is_what_ran() {
        let fx = folder_with_files();
        struct Works;
        impl Scanner for Works {
            fn name(&self) -> &'static str {
                "works"
            }
            fn scan(&self, root: &Path, ctx: &ScanContext) -> Result<EngineEnd, ScanError> {
                bs_scan::Walker.scan(root, ctx)
            }
        }
        let plan = Plan {
            backend: Backend::Btrfs,
            fallback: Fallback::None,
            reader: Reader::Stub(Box::new(Works)),
            mount_ids: Vec::new(),
        };
        let (chosen, mut tree, end) = run_chosen(plan, &fx.p("tree"));
        assert_eq!(end.unwrap(), EngineEnd::Completed);
        assert_eq!(chosen.outcome(), (Backend::Btrfs, Fallback::None));
        assert_eq!(files_below_root(&mut tree), 2);
    }

    // ----------------------------------------------------------- Windows

    const DRIVES: &[char] = &['C', 'D'];

    #[test]
    fn the_volume_is_named_from_the_drive_list_and_nothing_else() {
        for (root, want) in [
            (r"C:\", Some('C')),
            (r"\\?\C:\", Some('C')),
            (r"d:\", Some('D')),
            (r"\\?\d:\", Some('D')),
        ] {
            let target = ntfs_target(root, DRIVES);
            let want = want.map(|letter| NtfsTarget {
                device: PathBuf::from(format!(r"\\.\{letter}:")),
                root: PathBuf::from(format!(r"{letter}:\")),
            });
            assert_eq!(target, want, "{root}");
        }
    }

    #[test]
    fn a_path_from_the_request_never_becomes_the_volume_that_is_opened() {
        // Anything the request could say: other drives, folders, device names,
        // shares, images, volume GUIDs, odd spellings.
        for hostile in [
            r"E:\",
            r"C:\Windows",
            r"C:\Users\",
            r"C:\disk.img",
            r"C:",
            r"C",
            r"\\.\C:",
            r"\\.\C:\",
            r"\\.\PhysicalDrive0",
            r"\\.\GLOBALROOT\Device\HarddiskVolume1",
            r"\\?\GLOBALROOT\Device\HarddiskVolume1\",
            r"\\?\Volume{12345678-1234-1234-1234-123456789abc}\",
            r"\\server\share\",
            r"\\?\UNC\server\share\",
            r"\\?\C:\\",
            r"C:\\",
            r"C:/",
            r"CC:\",
            r"C:\.",
            r"\C:\",
            r"",
            "C:\\\0",
            "C:\\\u{fffd}",
            r"/dev/sda",
        ] {
            assert_eq!(ntfs_target(hostile, DRIVES), None, "{hostile:?}");
            for requested in [Backend::Auto, Backend::Ntfs] {
                let plan = choose_windows(requested, hostile, DRIVES, Some(1 << 40));
                assert!(
                    matches!(plan.reader, Reader::Walker),
                    "{hostile:?} {requested:?}"
                );
                assert_eq!(plan.backend, Backend::Walker);
            }
        }
        // With no NTFS drive found, no path is a volume.
        assert_eq!(ntfs_target(r"C:\", &[]), None);
        // Every plan that does run the reader opens exactly the volume of an
        // enumerated drive.
        for root in [r"C:\", r"\\?\D:\", r"c:\"] {
            let plan = choose_windows(Backend::Auto, root, DRIVES, None);
            let Reader::Ntfs { device, root, .. } = plan.reader else {
                panic!("{root:?}");
            };
            assert!(
                [r"\\.\C:", r"\\.\D:"].contains(&device.to_str().unwrap()),
                "{device:?}"
            );
            assert!([r"C:\", r"D:\"].contains(&root.to_str().unwrap()));
        }
    }

    #[test]
    fn the_ntfs_reader_is_used_for_a_drive_root_with_the_memory_the_system_reports() {
        let gib = 1u64 << 30;
        for (available, want) in [
            (Some(100 * gib), 4 * gib),
            (Some(2 * gib), gib),
            (Some(0), 0),
            (None, 4 * gib),
        ] {
            let plan = choose_windows(Backend::Auto, r"\\?\C:\", DRIVES, available);
            assert_eq!(
                (plan.backend, plan.fallback),
                (Backend::Ntfs, Fallback::None)
            );
            let Reader::Ntfs { options, .. } = plan.reader else {
                panic!("not the ntfs reader");
            };
            assert_eq!(options.max_memory_bytes, want, "{available:?}");
        }
    }

    #[test]
    fn what_is_not_a_drive_root_or_not_ntfs_is_walked_and_an_explicit_request_says_why() {
        for (requested, root, want) in [
            // A folder, or a drive that is not a fixed NTFS drive.
            (Backend::Auto, r"C:\Users", Fallback::None),
            (Backend::Ntfs, r"C:\Users", Fallback::NotSupported),
            (Backend::Auto, r"E:\", Fallback::None),
            (Backend::Ntfs, r"E:\", Fallback::NotSupported),
            // The walker was asked for.
            (Backend::Walker, r"C:\", Fallback::None),
            // Readers that exist only on Linux.
            (Backend::Ext4, r"C:\", Fallback::Unavailable),
            (Backend::Btrfs, r"C:\", Fallback::Unavailable),
            (Backend::Xfs, r"C:\", Fallback::Unavailable),
        ] {
            let plan = choose_windows(requested, root, DRIVES, None);
            assert_eq!(plan.backend, Backend::Walker, "{requested:?} {root}");
            assert_eq!(plan.fallback, want, "{requested:?} {root}");
            assert!(matches!(plan.reader, Reader::Walker));
        }
    }

    // ------------------------------------------------------------- Linux

    #[cfg(target_os = "linux")]
    mod linux {
        use super::*;
        use crate::mounts::{self, Mount};
        use crate::testutil::Fixture;

        /// [`choose`] for a root that the kernel reports on the mount the
        /// table says holds it (nothing hidden): the ordinary case.
        fn choose_in_view(
            requested: Backend,
            root: &Path,
            mounts: &[Mount],
            device_ok: &dyn Fn(&Path, (u32, u32)) -> bool,
            requested_mounts: &[u64],
        ) -> Plan {
            use std::os::unix::ffi::OsStrExt;
            let view = mounts::containing(mounts, root.as_os_str().as_bytes()).map(|m| m.id);
            choose(requested, root, mounts, device_ok, requested_mounts, view)
        }

        const TABLE: &str = "\
22 1 8:2 / / rw - ext4 /dev/sda2 rw
23 22 8:3 / /data rw - ext4 /dev/sda3 rw
24 22 8:4 /sub /part rw - ext4 /dev/sda4 rw
25 22 0:27 /@ /btr rw - btrfs /dev/sda5 rw
26 23 8:6 / /data/usb rw - vfat /dev/sdb1 rw
27 22 8:7 / /weird rw - ext4 /tmp/not-a-device rw
28 22 0:27 /@home /btr/home rw - btrfs /dev/sda5 rw
29 22 0:27 /@log /btr/log rw - btrfs /dev/sda5 rw
30 22 0:40 / /other rw - btrfs /dev/sdc1 rw
31 22 9:1 / /big rw - xfs /dev/md1 rw
32 22 9:2 /sub /bigpart rw - xfs /dev/md2 rw
33 22 0:60 / /btr/tmp rw - tmpfs tmpfs rw
";

        fn table() -> Vec<Mount> {
            mounts::parse(TABLE.as_bytes())
        }

        fn anything_under_dev(device: &Path, _: (u32, u32)) -> bool {
            device.starts_with("/dev")
        }

        fn choose_for(requested: Backend, root: &str) -> Plan {
            choose_in_view(
                requested,
                Path::new(root),
                &table(),
                &anything_under_dev,
                &[],
            )
        }

        fn reader_name(plan: &Plan) -> &'static str {
            match plan.reader {
                Reader::Walker => "walker",
                Reader::Ext4 { .. } => "ext4",
                Reader::Btrfs(_) => "btrfs",
                Reader::Xfs(_) => "xfs",
                _ => "other",
            }
        }

        #[test]
        fn a_whole_ext4_mount_is_read_raw_with_what_the_mount_table_says() {
            let plan = choose_for(Backend::Auto, "/data");
            assert_eq!(
                (plan.backend, plan.fallback),
                (Backend::Ext4, Fallback::None)
            );
            let Reader::Ext4 { device, options } = plan.reader else {
                panic!("not the ext4 reader");
            };
            assert_eq!(device, Path::new("/dev/sda3"));
            // Always passed, so a mount of part of a filesystem is refused.
            assert_eq!(options.mount_root.as_deref(), Some(&b"/"[..]));
            // Mounts below it are skipped; mounts elsewhere are not its business.
            assert_eq!(options.skip, vec![b"usb".to_vec()]);
            let Reader::Ext4 { options, .. } = choose_for(Backend::Ext4, "/").reader else {
                panic!("not the ext4 reader");
            };
            assert!(options.skip.contains(&b"data".to_vec()));
            assert!(options.skip.contains(&b"data/usb".to_vec()));
            assert!(options.skip.contains(&b"btr".to_vec()));
        }

        #[test]
        fn a_mount_of_part_of_a_filesystem_hands_its_root_to_the_reader() {
            let Reader::Ext4 { options, .. } = choose_for(Backend::Auto, "/part").reader else {
                panic!("not the ext4 reader");
            };
            assert_eq!(options.mount_root.as_deref(), Some(&b"/sub"[..]));
        }

        #[test]
        fn the_reader_follows_the_filesystem_type_of_the_mount() {
            for (root, want, backend) in [
                ("/data", "ext4", Backend::Ext4),
                ("/btr", "btrfs", Backend::Btrfs),
                // A subvolume mounted on its own is a mount point too.
                ("/btr/home", "btrfs", Backend::Btrfs),
                ("/other", "btrfs", Backend::Btrfs),
                ("/big", "xfs", Backend::Xfs),
            ] {
                let plan = choose_for(Backend::Auto, root);
                assert_eq!(reader_name(&plan), want, "{root}");
                assert_eq!(
                    (plan.backend, plan.fallback),
                    (backend, Fallback::None),
                    "{root}"
                );
                // The same, asked for by name.
                let plan = choose_for(backend, root);
                assert_eq!(reader_name(&plan), want, "{root} by name");
            }
        }

        #[test]
        fn everything_else_is_walked_and_an_explicit_request_says_why() {
            for (requested, root, want) in [
                // Not a mount point, or another kind of filesystem.
                (Backend::Auto, "/data/folder", Fallback::None),
                (Backend::Ext4, "/data/folder", Fallback::NotSupported),
                (Backend::Btrfs, "/btr/folder", Fallback::NotSupported),
                (Backend::Xfs, "/big/folder", Fallback::NotSupported),
                (Backend::Auto, "/data/usb", Fallback::None),
                (Backend::Ext4, "/data/usb", Fallback::NotSupported),
                (Backend::Ext4, "/btr", Fallback::NotSupported),
                (Backend::Btrfs, "/data", Fallback::NotSupported),
                (Backend::Xfs, "/btr", Fallback::NotSupported),
                (Backend::Btrfs, "/big", Fallback::NotSupported),
                // A device outside /dev is never read raw.
                (Backend::Ext4, "/weird", Fallback::NotSupported),
                (Backend::Auto, "/weird", Fallback::None),
                // A mount of part of an XFS filesystem: the bulk reader would
                // read everything for a part.
                (Backend::Auto, "/bigpart", Fallback::None),
                (Backend::Xfs, "/bigpart", Fallback::NotSupported),
                // The walker was asked for.
                (Backend::Walker, "/data", Fallback::None),
                (Backend::Walker, "/btr", Fallback::None),
                // The reader that exists only on Windows.
                (Backend::Ntfs, "/data", Fallback::Unavailable),
            ] {
                let plan = choose_for(requested, root);
                assert_eq!(plan.backend, Backend::Walker, "{requested:?} {root}");
                assert_eq!(plan.fallback, want, "{requested:?} {root}");
                assert!(matches!(plan.reader, Reader::Walker));
            }
        }

        #[test]
        fn a_device_the_check_refuses_is_not_read() {
            let plan = choose_in_view(
                Backend::Auto,
                Path::new("/data"),
                &table(),
                &|_, _| false,
                &[],
            );
            assert_eq!(plan.backend, Backend::Walker);
            assert!(matches!(plan.reader, Reader::Walker));
            // The check is for raw block devices; the ioctl readers have none.
            let plan = choose_in_view(
                Backend::Auto,
                Path::new("/btr"),
                &table(),
                &|_, _| false,
                &[],
            );
            assert_eq!(plan.backend, Backend::Btrfs);
        }

        #[test]
        fn a_mount_the_scan_root_is_not_on_is_not_the_one_chosen() {
            // The table lists a mount at /data, but `statx` says the folder is
            // on another mount (something was mounted over a shorter path and
            // hides it, or the table is not this namespace's). The table does
            // not describe what is there: no raw reader, no further mounts.
            let table = table();
            let data = mounts::at_exactly(&table, b"/data").unwrap().id;
            let btr = mounts::at_exactly(&table, b"/btr").unwrap().id;
            let hidden_under = 999;
            let by = |root: &str, requested: Backend, extra: &[u64], view: Option<u64>| {
                choose(
                    requested,
                    Path::new(root),
                    &table,
                    &anything_under_dev,
                    extra,
                    view,
                )
            };
            // On the mount the table names: the reader is chosen.
            let plan = by("/data", Backend::Auto, &[], Some(data));
            assert_eq!(plan.backend, Backend::Ext4);
            let plan = by("/btr", Backend::Auto, &[28], Some(btr));
            assert_eq!((plan.backend, plan.mount_ids), (Backend::Btrfs, vec![28]));
            // On another mount, or on one the kernel does not name.
            for view in [Some(hidden_under), Some(btr), Some(0), None] {
                for requested in [Backend::Auto, Backend::Ext4] {
                    let plan = by("/data", requested, &[], view);
                    assert_eq!(plan.backend, Backend::Walker, "{view:?} {requested:?}");
                    assert!(matches!(plan.reader, Reader::Walker));
                }
            }
            // An explicit request says it was not used; the automatic choice
            // has nothing to explain.
            assert_eq!(
                by("/data", Backend::Ext4, &[], Some(hidden_under)).fallback,
                Fallback::NotSupported
            );
            assert_eq!(
                by("/data", Backend::Auto, &[], Some(hidden_under)).fallback,
                Fallback::None
            );
            // The mounts of the same filesystem are not entered either: they
            // were found under a mount that is not in view.
            for requested in [Backend::Auto, Backend::Walker, Backend::Btrfs] {
                let plan = by("/btr", requested, &[28, 29], Some(hidden_under));
                assert!(plan.mount_ids.is_empty(), "{requested:?}");
                assert_eq!(plan.backend, Backend::Walker);
                let plan = by("/btr", requested, &[28, 29], None);
                assert!(plan.mount_ids.is_empty(), "{requested:?} on an old kernel");
            }
            // A folder inside a mount is judged by the mount that holds it.
            let plan = by("/btr/folder", Backend::Walker, &[28], Some(btr));
            assert_eq!(plan.mount_ids, vec![28]);
            let plan = by("/btr/folder", Backend::Walker, &[28], Some(hidden_under));
            assert!(plan.mount_ids.is_empty());
        }

        #[test]
        fn a_device_is_read_raw_only_when_it_is_the_one_the_table_shows_for_the_mount() {
            // The table shows 8:3 for /data and 8:2 for /; the check is given
            // the number of the mount that holds the root.
            let asked = std::cell::RefCell::new(Vec::new());
            let record = |device: &Path, number: (u32, u32)| {
                asked.borrow_mut().push((device.to_path_buf(), number));
                true
            };
            let plan = choose_in_view(Backend::Auto, Path::new("/data"), &table(), &record, &[]);
            assert_eq!(plan.backend, Backend::Ext4);
            assert_eq!(*asked.borrow(), vec![(PathBuf::from("/dev/sda3"), (8, 3))]);
            // A check that finds another number refuses the device.
            let wrong = |_: &Path, number: (u32, u32)| number != (8, 3);
            let plan = choose_in_view(Backend::Auto, Path::new("/data"), &table(), &wrong, &[]);
            assert_eq!(plan.backend, Backend::Walker);
            assert!(matches!(plan.reader, Reader::Walker));
        }

        #[test]
        fn device_numbers_are_compared_as_major_and_minor() {
            for (major, minor) in [(8u32, 3u32), (259, 1), (253, 0), (0, 0), (4095, 1_048_575)] {
                let rdev = libc::makedev(major, minor);
                assert!(is_device_number(rdev, (major, minor)));
                assert!(!is_device_number(rdev, (major, minor ^ 1)));
                assert!(!is_device_number(rdev, (major ^ 1, minor)));
                assert!(!is_device_number(rdev, (minor, major)) || major == minor);
            }
        }

        #[test]
        fn a_real_block_device_passes_only_with_its_own_number() {
            use std::os::unix::fs::{FileTypeExt, MetadataExt};
            let Some(device) = std::fs::read_dir("/dev")
                .into_iter()
                .flatten()
                .flatten()
                .map(|entry| entry.path())
                .find(|path| {
                    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_block_device())
                })
            else {
                return println!("NOT RUN: there is no block device node in /dev here");
            };
            let rdev = std::fs::metadata(&device).unwrap().rdev();
            let number = (libc::major(rdev), libc::minor(rdev));
            assert!(is_block_device_under_dev(&device, number), "{device:?}");
            assert!(!is_block_device_under_dev(
                &device,
                (number.0, number.1 ^ 1)
            ));
            assert!(!is_block_device_under_dev(
                &device,
                (number.0 ^ 1, number.1)
            ));
        }

        #[test]
        fn only_mounts_of_the_scan_roots_own_filesystem_may_be_entered() {
            let ids = |requested: &[u64], root: &str, backend| {
                choose_in_view(
                    backend,
                    Path::new(root),
                    &table(),
                    &anything_under_dev,
                    requested,
                )
                .mount_ids
            };
            // The subvolumes of the same btrfs filesystem (same device number),
            // asked for by the request, are kept ...
            assert_eq!(ids(&[28, 29], "/btr", Backend::Auto), vec![28, 29]);
            // ... a tmpfs mounted in the middle of them, another btrfs
            // filesystem, a vfat stick, an id that is no mount, and the
            // filesystem of the root are not.
            assert_eq!(
                ids(&[28, 33, 30, 26, 999, 22], "/btr", Backend::Auto),
                vec![28]
            );
            // The same for a walk: the folder is judged by the mount that
            // holds it.
            assert_eq!(ids(&[28, 30], "/btr/folder", Backend::Walker), vec![28]);
            assert_eq!(
                ids(&[23, 24, 28], "/data", Backend::Walker),
                Vec::<u64>::new()
            );
            // Nothing requested, nothing entered.
            assert!(ids(&[], "/btr", Backend::Auto).is_empty());
            // A root the table does not know cannot vouch for any mount.
            let nowhere = choose_in_view(
                Backend::Walker,
                Path::new("relative"),
                &table(),
                &anything_under_dev,
                &[28],
            );
            assert!(nowhere.mount_ids.is_empty());
            let unreadable = choose_in_view(
                Backend::Walker,
                Path::new("/btr"),
                &[],
                &anything_under_dev,
                &[28],
            );
            assert!(unreadable.mount_ids.is_empty());
        }

        #[test]
        fn a_mount_that_another_mount_already_shows_is_not_entered() {
            // A bind mount of a folder the scan reaches through the mount it
            // starts from, and a second mount of the top of the filesystem,
            // are the same files again. A subvolume mounted on the folder where
            // the top level shows it is in the way, so it is entered.
            let text = "\
1 0 0:5 / / rw - ext4 /dev/sda1 rw
40 1 0:70 / /pool rw - btrfs /dev/sdd1 rw
41 40 0:70 /home /pool/home rw - btrfs /dev/sdd1 rw
42 40 0:70 /data /pool/deep/data rw - btrfs /dev/sdd1 rw
43 1 0:71 / /twice rw - btrfs /dev/sde1 rw
44 1 0:71 / /twice-again rw - btrfs /dev/sde1 rw
50 1 8:9 / /vol rw - ext4 /dev/sda9 rw
51 50 8:9 /docs /vol/docs-again rw - ext4 /dev/sda9 rw
52 50 8:9 /other /vol/other rw - ext4 /dev/sda9 rw
60 1 8:11 /sub /part rw - ext4 /dev/sda11 rw
61 60 8:11 /other /part/other rw - ext4 /dev/sda11 rw
62 60 8:11 /sub/inner /part/elsewhere rw - ext4 /dev/sda11 rw
63 60 8:11 /sub/inner /part/inner rw - ext4 /dev/sda11 rw
";
            let table = mounts::parse(text.as_bytes());
            let ids = |root: &str, requested: &[u64]| {
                choose_in_view(
                    Backend::Walker,
                    Path::new(root),
                    &table,
                    &anything_under_dev,
                    requested,
                )
                .mount_ids
            };
            // Both of the subvolume mounts are asked for; only the one in the
            // way of the top level is entered.
            assert_eq!(ids("/pool", &[41, 42]), vec![41]);
            // A second mount of the whole filesystem shows everything again.
            assert!(ids("/twice", &[44]).is_empty());
            // A bind mount of a folder inside the mount the scan starts from.
            assert_eq!(ids("/vol", &[51]), Vec::<u64>::new());
            // Mounts of part of a filesystem: one that shows a folder the start
            // mount does not show is kept, and so is one that is in the way of
            // the folder the start mount would show; one that shows a folder
            // the start mount shows elsewhere is not.
            assert_eq!(ids("/part", &[61, 62, 63]), vec![61, 63]);
            // Asked for twice, covered once: still not entered.
            assert!(ids("/vol", &[51, 51]).is_empty());
        }

        #[test]
        fn the_scans_start_decides_which_mounts_cover_which() {
            // `/` is the subvolume @, @home is mounted at /home, and the top
            // level is mounted elsewhere. A scan of `/` has to enter /home
            // whether or not the top level is mounted; it must not enter the
            // top level, which shows the start of the scan again.
            let text = "\
1 0 0:27 /@ / rw - btrfs /dev/sda3 rw
2 1 0:27 /@home /home rw - btrfs /dev/sda3 rw
3 1 0:27 / /mnt/top rw - btrfs /dev/sda3 rw
";
            let ids = |text: &str, requested: &[u64]| {
                choose_in_view(
                    Backend::Walker,
                    Path::new("/"),
                    &mounts::parse(text.as_bytes()),
                    &anything_under_dev,
                    requested,
                )
                .mount_ids
            };
            assert_eq!(ids(text, &[2]), vec![2]);
            assert_eq!(ids(text, &[2, 3]), vec![2]);
            assert!(ids(text, &[3]).is_empty());
            let without_top = "\
1 0 0:27 /@ / rw - btrfs /dev/sda3 rw
2 1 0:27 /@home /home rw - btrfs /dev/sda3 rw
";
            assert_eq!(ids(without_top, &[2]), vec![2]);
        }

        #[test]
        fn only_block_devices_under_dev_pass_the_real_check() {
            let fx = Fixture::new("device-check");
            fx.write("not-a-device", "x");
            assert!(!is_block_device_under_dev(&fx.p("not-a-device"), (0, 0)));
            assert!(!is_block_device_under_dev(Path::new("/etc/passwd"), (0, 0)));
            assert!(!is_block_device_under_dev(Path::new("/dev/null"), (0, 0)));
            assert!(!is_block_device_under_dev(
                Path::new("/dev/does-not-exist"),
                (0, 0)
            ));
            assert!(!is_block_device_under_dev(
                Path::new("/dev/../etc/passwd"),
                (0, 0)
            ));
        }

        #[test]
        fn a_btrfs_or_xfs_reader_that_cannot_read_the_folder_falls_back_and_says_so() {
            // The fixture is not on btrfs or XFS here (and the helper is not
            // root), so the real readers refuse; if it were, they would read
            // it, and the file count below would still be right.
            let fx = folder_with_files();
            for (backend, reader) in [
                (
                    Backend::Btrfs,
                    Reader::Btrfs(bs_scan_btrfs::BtrfsOptions::default()),
                ),
                (
                    Backend::Xfs,
                    Reader::Xfs(bs_scan_xfs::XfsOptions::default()),
                ),
            ] {
                let plan = Plan {
                    backend,
                    fallback: Fallback::None,
                    reader,
                    mount_ids: Vec::new(),
                };
                let (chosen, mut tree, end) = run_chosen(plan, &fx.p("tree"));
                assert_eq!(end.unwrap(), EngineEnd::Completed);
                let (ran, why) = chosen.outcome();
                if ran == Backend::Walker {
                    assert_eq!(why, Fallback::NotSupported, "{backend:?}");
                } else {
                    assert_eq!(ran, backend);
                }
                assert_eq!(files_below_root(&mut tree), 2, "{backend:?}");
            }
        }

        fn ext4_image(fx: &Fixture) -> Option<PathBuf> {
            fx.write("src/a/one.txt", "1");
            fx.write("src/a/two.txt", "22");
            fx.write("src/three.txt", "333");
            let image = fx.p("disk.img");
            let status = std::process::Command::new("mke2fs")
                .args(["-q", "-F", "-t", "ext4", "-d"])
                .arg(fx.p("src"))
                .arg(&image)
                .arg("4M")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .ok()?;
            status.success().then_some(image)
        }

        fn ext4_plan(device: PathBuf, options: bs_scan_ext4::Ext4Options) -> Plan {
            Plan {
                backend: Backend::Ext4,
                fallback: Fallback::None,
                reader: Reader::Ext4 { device, options },
                mount_ids: Vec::new(),
            }
        }

        #[test]
        fn the_raw_reader_runs_when_the_plan_has_one() {
            let fx = Fixture::new("raw-ok");
            let Some(image) = ext4_image(&fx) else {
                return println!("NOT RUN: mke2fs is not available");
            };
            let plan = ext4_plan(image, bs_scan_ext4::Ext4Options::default());
            let (chosen, mut tree, end) = run_chosen(plan, fx.path());
            assert_eq!(end.unwrap(), EngineEnd::Completed);
            assert_eq!(chosen.outcome(), (Backend::Ext4, Fallback::None));
            assert_eq!(files_below_root(&mut tree), 3);
        }

        #[test]
        fn a_filesystem_that_needs_too_much_memory_falls_back_to_the_walker() {
            let fx = Fixture::new("raw-too-large");
            let Some(image) = ext4_image(&fx) else {
                return println!("NOT RUN: mke2fs is not available");
            };
            let options = bs_scan_ext4::Ext4Options {
                max_memory_bytes: 1,
                ..bs_scan_ext4::Ext4Options::default()
            };
            // The walker reads the folder that holds the image: the fixture.
            let (chosen, _tree, end) = run_chosen(ext4_plan(image, options), &fx.p("src"));
            assert_eq!(end.unwrap(), EngineEnd::Completed);
            assert_eq!(chosen.outcome(), (Backend::Walker, Fallback::TooLarge));
        }

        #[test]
        fn a_mount_of_part_of_a_filesystem_and_a_non_ext4_device_fall_back_too() {
            let fx = Fixture::new("raw-refused");
            let Some(image) = ext4_image(&fx) else {
                return println!("NOT RUN: mke2fs is not available");
            };
            let partial = bs_scan_ext4::Ext4Options {
                mount_root: Some(b"/sub".to_vec()),
                ..bs_scan_ext4::Ext4Options::default()
            };
            fx.write("zeros.img", &"\0".repeat(8192));
            for (device, options, want) in [
                (image, partial, Fallback::PartialMount),
                (
                    fx.p("zeros.img"),
                    bs_scan_ext4::Ext4Options::default(),
                    Fallback::NotSupported,
                ),
                (
                    fx.p("no-such-device"),
                    bs_scan_ext4::Ext4Options::default(),
                    Fallback::NotSupported,
                ),
            ] {
                let (chosen, _tree, end) = run_chosen(ext4_plan(device, options), &fx.p("src"));
                assert_eq!(end.unwrap(), EngineEnd::Completed);
                assert_eq!(chosen.outcome(), (Backend::Walker, want));
            }
        }
    }
}
