//! Where each drive's Recycle Bin or Trash is, and how much it holds.
//!
//! This module only reads. [`start`] measures one drive's bin on a worker
//! thread and returns at once, with a way to watch the progress and to cancel;
//! [`measure`] is the same work done on the calling thread, for callers that
//! already run off the interface thread.
//!
//! # What is counted
//!
//! The bytes are counted the way the scanner counts them, so the bin segment
//! of the capacity bar and the rest of the map agree:
//!
//! - **allocated** bytes are the space the filesystem reserved (blocks, not
//!   file sizes). This is what emptying the bin gives back.
//! - **logical** bytes are the sizes of the files. A folder adds nothing of
//!   its own to them.
//! - a file with several hard links is counted once, and counted in
//!   [`BinInfo::shared_files`], because deleting it from the bin may free
//!   nothing if another name for it lives elsewhere.
//! - nothing is followed: a link in the bin counts as a link, and a folder on
//!   another filesystem is not entered.
//!
//! # Linux
//!
//! The bin is the freedesktop.org Trash of the user, on each drive:
//!
//! - the home Trash (`$XDG_DATA_HOME/Trash`, by default
//!   `~/.local/share/Trash`) belongs to the drive that holds it;
//! - on every drive, `$topdir/.Trash/$uid` when `$topdir/.Trash` is a real
//!   folder (not a link) with the sticky bit, and `$topdir/.Trash-$uid` when it
//!   exists.
//!
//! An *item* is an `info/NAME.trashinfo` file with a `files/NAME` beside it.
//! Entries of one without the other are reported as [`BinProblem`]s and still
//! count towards the bytes, because they occupy space. The bytes cover
//! `files/`, `info/` and `directorysizes`, which is what the Trash
//! specification says is emptied. The `directorysizes` cache is not used for
//! the figures: it records one number per folder, not the allocated and
//! logical sizes both, and anything may have changed a folder since it was
//! written.
//!
//! # Windows
//!
//! The size and item count are the system's own (`SHQueryRecycleBinW`), which
//! reports the sizes of the original files, so there is no allocated figure
//! ([`BinInfo::allocated_bytes`] is `None`). The folder of the current user's
//! items, `X:\$Recycle.Bin\<user SID>`, is located without opening anyone
//! else's folder. The size limit and the delete-instead-of-recycle switch the
//! user set for the volume are read from the registry, for the capacity check
//! that a delete needs.

use crate::drives::{Drive, DriveId};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod walk;
#[cfg(windows)]
mod windows;
#[cfg(any(windows, test))]
mod winpure;

/// What the system calls its bin.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BinName {
    /// "Recycle Bin", on Windows.
    RecycleBin,
    /// "Trash", on Linux.
    Trash,
}

impl BinName {
    /// The name the current platform uses.
    pub const fn platform() -> BinName {
        if cfg!(windows) {
            BinName::RecycleBin
        } else {
            BinName::Trash
        }
    }

    /// The name as written in texts.
    pub const fn as_str(self) -> &'static str {
        match self {
            BinName::RecycleBin => "Recycle Bin",
            BinName::Trash => "Trash",
        }
    }
}

/// What a place is, in the system's own scheme.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LocationRole {
    /// The user's Trash in their data folder.
    HomeTrash,
    /// `$topdir/.Trash/$uid`, inside the shared sticky `.Trash` folder.
    SharedVolumeTrash,
    /// `$topdir/.Trash-$uid`.
    VolumeTrash,
    /// `X:\$Recycle.Bin\<user SID>`.
    UserRecycleFolder,
}

/// One folder that holds recycled or trashed items.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BinLocation {
    pub path: PathBuf,
    pub role: LocationRole,
}

/// Why a [`BinInfo`] has something to say beyond its numbers.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BinProblem {
    /// `$topdir/.Trash` exists but may not be used, so the specification asks
    /// to use `.Trash-$uid` instead.
    SharedTrashUnusable {
        path: PathBuf,
        reason: UnusableReason,
    },
    /// A folder that should belong to the user belongs to someone else, and is
    /// not read.
    NotOwnedByUser { path: PathBuf },
    /// A link stands where a folder should be, and was not followed.
    NotFollowed { path: PathBuf },
    /// A `.trashinfo` file has no item beside it in `files/`.
    OrphanInfo { name: Vec<u8> },
    /// An item in `files/` has no `.trashinfo` file.
    OrphanFile { name: Vec<u8> },
    /// Something could not be read.
    Unreadable { path: PathBuf, kind: io::ErrorKind },
    /// A folder inside the bin is on another filesystem and was not entered.
    OtherFilesystem { path: PathBuf },
    /// Folders are nested deeper than the measurement goes.
    TooDeep { path: PathBuf },
    /// The measurement was cancelled; the figures are what was counted so far.
    Cancelled,
    /// The bin could not be measured: the system's query failed (`code` is its
    /// error code) or the worker ended without an answer (`-1`).
    QueryFailed { code: i32 },
}

impl BinProblem {
    /// Whether the figures leave out something that is in the bin.
    pub fn makes_figures_incomplete(&self) -> bool {
        !matches!(
            self,
            BinProblem::SharedTrashUnusable { .. }
                | BinProblem::NotOwnedByUser { .. }
                | BinProblem::OrphanInfo { .. }
                | BinProblem::OrphanFile { .. }
        )
    }
}

/// Why `$topdir/.Trash` cannot be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum UnusableReason {
    /// It is a symbolic link.
    Symlink,
    /// It is not a folder.
    NotDirectory,
    /// The sticky bit is not set.
    NotSticky,
}

/// The user's settings for how a volume's Recycle Bin behaves. Only Windows
/// has them; the fields are `None` where there is nothing stored or nothing
/// that could be read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BinPolicy {
    /// The most the bin holds before older items are removed or a delete
    /// bypasses it. `None` means the setting is not stored (the system default
    /// applies) or could not be read.
    pub max_bytes: Option<u64>,
    /// Deletes skip the bin and remove files at once ("Don't move files to the
    /// Recycle Bin"). `None` when not stored.
    pub delete_permanently: Option<bool>,
}

/// What one drive's bin holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BinInfo {
    /// The drive the bin is on.
    pub drive: DriveId,
    pub name: BinName,
    /// The folders that were looked at and exist; empty when the drive has no
    /// bin yet.
    pub locations: Vec<BinLocation>,
    /// Bytes reserved on the drive by what the bin holds. `None` on Windows,
    /// whose query gives file sizes only.
    pub allocated_bytes: Option<u64>,
    /// The sizes of the files in the bin, added up.
    pub logical_bytes: u64,
    /// The number of items.
    pub items: u64,
    /// Files with more than one hard link, each counted once in the bytes.
    pub shared_files: u64,
    /// Whether the figures cover everything in the bin. `false` when
    /// something could not be read or the measurement was cut short.
    pub complete: bool,
    /// What was found along the way. At most [`MAX_PROBLEMS`] are kept;
    /// `problems_omitted` counts the rest.
    pub problems: Vec<BinProblem>,
    pub problems_omitted: u64,
    pub policy: BinPolicy,
}

/// The most problems a [`BinInfo`] lists.
pub const MAX_PROBLEMS: usize = 200;

impl BinInfo {
    pub(crate) fn empty(drive: DriveId) -> Self {
        Self {
            drive,
            name: BinName::platform(),
            locations: Vec::new(),
            allocated_bytes: None,
            logical_bytes: 0,
            items: 0,
            shared_files: 0,
            complete: true,
            problems: Vec::new(),
            problems_omitted: 0,
            policy: BinPolicy::default(),
        }
    }

    /// Bytes the bin takes on the drive: the allocated figure where there is
    /// one, otherwise the logical figure.
    pub fn bytes_for_display(&self) -> u64 {
        self.allocated_bytes.unwrap_or(self.logical_bytes)
    }
}

/// Collects problems up to a limit.
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Default)]
pub(crate) struct Problems {
    pub list: Vec<BinProblem>,
    pub omitted: u64,
    pub incomplete: bool,
}

#[cfg(any(target_os = "linux", test))]
impl Problems {
    pub fn add(&mut self, problem: BinProblem) {
        self.incomplete |= problem.makes_figures_incomplete();
        if self.list.len() < MAX_PROBLEMS {
            self.list.push(problem);
        } else {
            self.omitted += 1;
        }
    }
}

/// Lets the caller watch and stop a measurement.
#[derive(Debug, Default)]
pub struct BinControl {
    cancelled: AtomicBool,
    entries: AtomicU64,
    allocated: AtomicU64,
}

/// How far a measurement has come.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BinProgress {
    /// Files and folders looked at so far.
    pub entries: u64,
    /// Allocated bytes counted so far.
    pub allocated_bytes: u64,
}

impl BinControl {
    pub fn new() -> Self {
        Self::default()
    }

    /// Asks the measurement to stop. It keeps what it has counted and says so.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    pub fn progress(&self) -> BinProgress {
        BinProgress {
            entries: self.entries.load(Ordering::Relaxed),
            allocated_bytes: self.allocated.load(Ordering::Relaxed),
        }
    }

    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn count(&self, allocated: u64) {
        self.entries.fetch_add(1, Ordering::Relaxed);
        self.allocated.fetch_add(allocated, Ordering::Relaxed);
    }
}

/// Measures the bin of `drive` on the calling thread.
///
/// On Linux this walks the Trash folders and takes time in proportion to their
/// size; a bin of 100,000 entries takes a fraction of a second when the files
/// are in the cache. Call it off the interface thread, or use [`start`].
pub fn measure(drive: &Drive, control: &BinControl) -> BinInfo {
    #[cfg(target_os = "linux")]
    {
        linux::measure_drive(drive, control)
    }
    #[cfg(windows)]
    {
        windows::measure_drive(drive, control)
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = control;
        BinInfo::empty(drive.id.clone())
    }
}

/// Finds the folders that hold the bin of `drive` without measuring them.
pub fn locate(drive: &Drive) -> Vec<BinLocation> {
    #[cfg(target_os = "linux")]
    {
        linux::locate_drive(drive)
    }
    #[cfg(windows)]
    {
        windows::locate_drive(drive)
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = drive;
        Vec::new()
    }
}

/// A measurement running on its own thread.
///
/// Dropping it cancels the measurement and does not wait for it.
#[derive(Debug)]
pub struct BinQuery {
    control: Arc<BinControl>,
    drive: DriveId,
    receiver: mpsc::Receiver<BinInfo>,
}

/// Starts measuring the bin of `drive` on a new thread.
pub fn start(drive: &Drive) -> BinQuery {
    let control = Arc::new(BinControl::new());
    let (sender, receiver) = mpsc::channel();
    let owned = drive.clone();
    let worker_control = Arc::clone(&control);
    let spawned = thread::Builder::new()
        .name("bs-bin-query".to_owned())
        .spawn(move || {
            let info = measure(&owned, &worker_control);
            // The query may have been dropped.
            let _ = sender.send(info);
        });
    // When the thread cannot be started the sender is gone with the closure,
    // and waiting reports that the query failed.
    let _ = spawned;
    BinQuery {
        control,
        drive: drive.id.clone(),
        receiver,
    }
}

impl BinQuery {
    pub fn progress(&self) -> BinProgress {
        self.control.progress()
    }

    /// Asks the measurement to stop. The result still arrives, marked
    /// cancelled and incomplete.
    pub fn cancel(&self) {
        self.control.cancel();
    }

    /// The result, if the measurement has finished. Gives it once.
    pub fn poll(&mut self) -> Option<BinInfo> {
        match self.receiver.try_recv() {
            Ok(info) => Some(info),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(self.failed()),
        }
    }

    /// Waits for the result for at most `timeout`.
    pub fn wait_timeout(&mut self, timeout: Duration) -> Option<BinInfo> {
        match self.receiver.recv_timeout(timeout) {
            Ok(info) => Some(info),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => Some(self.failed()),
        }
    }

    /// Waits for the result.
    pub fn wait(self) -> BinInfo {
        match self.receiver.recv() {
            Ok(info) => info,
            Err(_) => self.failed(),
        }
    }

    /// The result when the worker is gone without having answered.
    fn failed(&self) -> BinInfo {
        let mut info = BinInfo::empty(self.drive.clone());
        info.complete = false;
        info.problems.push(BinProblem::QueryFailed { code: -1 });
        info
    }
}

impl Drop for BinQuery {
    fn drop(&mut self) {
        self.control.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_match_the_platform_wording() {
        assert_eq!(BinName::RecycleBin.as_str(), "Recycle Bin");
        assert_eq!(BinName::Trash.as_str(), "Trash");
        let expected = if cfg!(windows) {
            BinName::RecycleBin
        } else {
            BinName::Trash
        };
        assert_eq!(BinName::platform(), expected);
    }

    #[test]
    fn only_lost_figures_make_a_result_incomplete() {
        let path = PathBuf::from("/x");
        let complete = [
            BinProblem::SharedTrashUnusable {
                path: path.clone(),
                reason: UnusableReason::NotSticky,
            },
            BinProblem::NotOwnedByUser { path: path.clone() },
            BinProblem::OrphanInfo {
                name: b"a".to_vec(),
            },
            BinProblem::OrphanFile {
                name: b"a".to_vec(),
            },
        ];
        for problem in complete {
            assert!(!problem.makes_figures_incomplete(), "{problem:?}");
        }
        let incomplete = [
            BinProblem::NotFollowed { path: path.clone() },
            BinProblem::Unreadable {
                path: path.clone(),
                kind: io::ErrorKind::PermissionDenied,
            },
            BinProblem::OtherFilesystem { path: path.clone() },
            BinProblem::TooDeep { path },
            BinProblem::Cancelled,
            BinProblem::QueryFailed { code: -1 },
        ];
        for problem in incomplete {
            assert!(problem.makes_figures_incomplete(), "{problem:?}");
        }
    }

    #[test]
    fn problems_are_kept_up_to_a_limit_and_the_rest_counted() {
        let mut problems = Problems::default();
        for i in 0..(MAX_PROBLEMS + 25) {
            problems.add(BinProblem::OrphanFile {
                name: i.to_string().into_bytes(),
            });
        }
        assert_eq!(problems.list.len(), MAX_PROBLEMS);
        assert_eq!(problems.omitted, 25);
        // Orphans alone do not make the figures incomplete.
        assert!(!problems.incomplete);
        problems.add(BinProblem::Cancelled);
        assert!(problems.incomplete);
        assert_eq!(problems.omitted, 26);
    }

    #[test]
    fn display_bytes_prefer_the_allocated_figure() {
        let mut info = BinInfo::empty(DriveId::new("x"));
        info.logical_bytes = 10;
        assert_eq!(info.bytes_for_display(), 10);
        info.allocated_bytes = Some(4096);
        assert_eq!(info.bytes_for_display(), 4096);
    }

    #[test]
    fn control_counts_and_cancels() {
        let control = BinControl::new();
        assert!(!control.is_cancelled());
        control.count(4096);
        control.count(512);
        assert_eq!(
            control.progress(),
            BinProgress {
                entries: 2,
                allocated_bytes: 4608
            }
        );
        control.cancel();
        assert!(control.is_cancelled());
    }
}
