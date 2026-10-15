//! The XFS reader, used by the elevated helper on Linux. The crate is empty
//! on other systems.
//!
//! It reads a mounted XFS file system in two parts. The sizes come from the
//! kernel's bulk inode read (`XFS_IOC_BULKSTAT`), which hands over every inode
//! of the file system in inode order, with its size, its blocks, its link
//! count, its mode and its times, without looking at any directory entry: far
//! fewer requests than a `statx` per file. The names come from a walk of the
//! directories alone, which reads each directory once and asks nothing of the
//! files in it. The two are joined by inode number. Entries that the bulk read
//! did not hold (made since) are looked up directly. It never opens a file,
//! and it never follows a link.
//!
//! # Distrust
//!
//! The bulk reply and the directory listings come from the kernel, but the
//! reader treats them as hostile all the same: a count the buffer cannot hold,
//! inode numbers that do not increase, a record that does not fit and a name
//! that cannot exist are errors or marks, never a panic or an endless read; the
//! memory the tables take is charged as they grow and held to a limit the
//! caller sets; the walk uses explicit stacks, opens directories without
//! following links, and stays on the mounts the scan may be in. An entry that
//! is dropped because it contradicts the inode table marks its directory
//! unreadable, so no total above it claims to be exact.
//!
//! # Hard links
//!
//! A file with more than one name is flagged and keyed by device and inode
//! number, so the tree counts its bytes once.
//!
//! # What it does not do
//!
//! * It does not use parent pointers (XFS since Linux 6.10 can keep them), so
//!   it needs the directory walk for names on every file system.
//! * The sizes are those of the inode table as the kernel holds it. Data not
//!   yet written out (delayed allocation) is not in the blocks the bulk read
//!   reports, so a file written a moment ago can show fewer blocks than
//!   `statx` would; flush the file system first when that matters.
//! * It reads the whole file system's inode table even when the scan starts
//!   below its root.
#![cfg(all(target_os = "linux", target_endian = "little"))]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation
)]

mod bulkstat;
mod emit;
mod error;
mod le;
mod names;
mod sys;
mod walk;

#[cfg(test)]
mod proptests;

pub use bulkstat::BulkStat;
pub use error::{Diagnostics, MAX_SAMPLES, Problem, ProblemKind, XfsError};
pub use names::{EntryStat, FsNames, Names, OpenedDir, RawName};
pub use sys::IoctlInodes;

use bs_scan::{ScanContext, ScanEnd, ScanError, Scanner};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use walk::{Stat, Table};

/// The most memory the reader's tables may take, whatever the machine has.
pub const MEMORY_CEILING: u64 = 4 << 30;

/// Something that can hand over the inodes of the file system in bulk.
pub trait Inodes: Sync {
    /// Calls `each` for every inode of the file system in ascending order of
    /// inode number, until it returns `false`.
    fn each(&self, each: &mut dyn FnMut(BulkStat) -> bool) -> Result<(), XfsError>;
}

/// What a caller decides about a read.
#[derive(Clone, Debug)]
pub struct XfsOptions {
    /// The most memory the in-memory tables may take. A file system that
    /// needs more is refused with [`XfsError::TooLarge`] before it can exhaust
    /// the machine; the caller falls back to the walker. [`XfsScanner`] lowers
    /// it to half of the memory available when the scan starts (see
    /// [`memory_cap`]); the figure used is in [`XfsReport::memory_cap_bytes`].
    pub max_memory_bytes: u64,
    /// Threads that walk the directories. `None` picks a number from the
    /// cores.
    pub threads: Option<usize>,
}

impl Default for XfsOptions {
    fn default() -> Self {
        XfsOptions {
            max_memory_bytes: MEMORY_CEILING,
            threads: None,
        }
    }
}

/// The memory limit for a read that starts now: `ceiling`, or half of the
/// memory the machine reports as available if that is less. The reader may
/// run as root with the whole machine to itself; running it out of memory
/// would take the user's session down with it.
///
/// When the available memory cannot be read, `ceiling` is returned unchanged.
pub fn memory_cap(ceiling: u64) -> u64 {
    let available = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|text| mem_available(&text));
    cap_from(ceiling, available)
}

fn cap_from(ceiling: u64, available: Option<u64>) -> u64 {
    match available {
        Some(bytes) => ceiling.min(bytes / 2),
        None => ceiling,
    }
}

/// `MemAvailable` from the text of `/proc/meminfo`, in bytes.
fn mem_available(text: &str) -> Option<u64> {
    let line = text.lines().find_map(|l| l.strip_prefix("MemAvailable:"))?;
    let mut words = line.split_whitespace();
    let number: u64 = words.next()?.parse().ok()?;
    match words.next() {
        Some("kB") => number.checked_mul(1024),
        None => Some(number),
        Some(_) => None,
    }
}

/// Inodes and bytes that are on the file system but not in the scan's tree.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Unaccounted {
    /// How many inodes.
    pub inodes: u64,
    /// Their allocated size in bytes.
    pub allocated: u64,
}

/// What a read found besides the tree.
#[derive(Clone, Debug, Default)]
pub struct XfsReport {
    /// Inodes the bulk read returned.
    pub inodes: u64,
    /// Directories listed.
    pub directories: u64,
    /// Entries listed.
    pub entries: u64,
    /// Entries the bulk read did not hold, looked up one by one.
    pub looked_up: u64,
    /// Inodes of the file system that no entry of the scan reached
    /// (including everything outside the scan when it starts below the
    /// root, files removed while open, and the file system's own inodes).
    pub unreachable: Unaccounted,
    /// The memory limit this read ran under, in bytes.
    pub memory_cap_bytes: u64,
    /// The memory the tables were charged, in bytes.
    pub memory_used_bytes: u64,
    /// Entries or directories reported unreadable. This includes a directory
    /// marked unreadable because one of its entries was dropped.
    pub unreadable_entries: u64,
    /// Entries that named an inode that was gone.
    pub vanished_entries: u64,
    /// Every inconsistency met.
    pub diagnostics: Diagnostics,
    /// The read stopped early because it was cancelled.
    pub cancelled: bool,
    /// Threads that walked the directories.
    pub threads: usize,
    /// Time spent reading the inode table.
    pub inode_time: Duration,
    /// Time spent walking the directories.
    pub walk_time: Duration,
    /// Time spent turning that into batches and handing them on.
    pub emit_time: Duration,
}

fn default_threads() -> usize {
    std::thread::available_parallelism().map_or(2, |n| n.get().min(8))
}

/// Reads the inode table through `inodes` and the directories through
/// `names`, and sends the tree to `ctx` as batches. The first record is the
/// root of `names`, called `root_name`.
///
/// `Err` only when there is no tree to send: the bulk read failed outright,
/// the root cannot be opened, or more metadata than the memory limit allows.
/// Anything less is reported in the [`XfsReport`] and marked in the tree.
pub fn read_filesystem<N: Names>(
    inodes: &dyn Inodes,
    names: &N,
    root_name: &[u8],
    options: &XfsOptions,
    ctx: &ScanContext,
) -> Result<(ScanEnd, XfsReport), XfsError> {
    let mut report = XfsReport {
        memory_cap_bytes: options.max_memory_bytes,
        ..XfsReport::default()
    };
    let limit = options.max_memory_bytes;

    // The inode table, charged as it grows.
    let begun = Instant::now();
    let mut table = Table { rows: Vec::new() };
    let mut charged = 0u64;
    let mut failure: Option<XfsError> = None;
    let mut stopped = false;
    inodes.each(&mut |b: BulkStat| {
        if ctx.is_cancelled() {
            stopped = true;
            return false;
        }
        if let Err(e) = make_room(&mut table.rows, &mut charged, limit) {
            failure = Some(e);
            return false;
        }
        table.rows.push(Stat::from(b));
        true
    })?;
    if let Some(e) = failure {
        return Err(e);
    }
    report.inode_time = begun.elapsed();
    if stopped {
        report.cancelled = true;
        return Ok((ScanEnd::Cancelled, report));
    }
    report.inodes = table.rows.len() as u64;

    let begun = Instant::now();
    let threads = options.threads.unwrap_or_else(default_threads).max(1);
    report.threads = threads;
    let walked = walk::walk(
        names,
        &ctx.options().boundary,
        &table,
        threads,
        limit,
        charged,
        &|| ctx.is_cancelled(),
    )?;
    report.walk_time = begun.elapsed();
    let Some(walked) = walked else {
        report.cancelled = true;
        return Ok((ScanEnd::Cancelled, report));
    };
    report.directories = walked.listings.len() as u64;
    report.entries = walked.listings.iter().map(|l| l.entries.len() as u64).sum();
    report.looked_up = walked.fallback_stats;
    report.memory_used_bytes = walked.memory;

    let begun = Instant::now();
    let mut emitter = emit::Emitter::new(&walked, &table, ctx, limit)?;
    let end = emitter.run(root_name)?;
    let (unreachable, _) = emitter.account_for_the_rest();
    report.emit_time = begun.elapsed();
    report.unreachable = unreachable;
    report.unreadable_entries = emitter.unreadable_entries;
    report.vanished_entries = emitter.vanished_entries;
    report.cancelled = emitter.cancelled;
    let mut diag = walked.diag.clone();
    diag.absorb(&emitter.diag);
    report.diagnostics = diag;
    Ok((end, report))
}

/// Makes room for one more row, charging the memory the vector really takes.
/// A growing vector copies itself, and for that moment the old and the new
/// storage both exist, so the new storage must fit under the limit on top of
/// everything charged so far.
fn make_room(rows: &mut Vec<Stat>, charged: &mut u64, limit: u64) -> Result<(), XfsError> {
    const OVER: XfsError =
        XfsError::TooLarge("the inode table needs more memory than the limit allows");
    if rows.len() < rows.capacity() {
        return Ok(());
    }
    let cap = rows.capacity();
    let new_cap = cap.saturating_add(cap / 2).max(1024);
    let size = std::mem::size_of::<Stat>() as u64;
    let old = (cap as u64).saturating_mul(size);
    let new = (new_cap as u64).saturating_mul(size);
    if charged.saturating_add(new) > limit {
        return Err(OVER);
    }
    rows.try_reserve_exact(new_cap.saturating_sub(rows.len()))
        .map_err(|_| OVER)?;
    *charged = charged.saturating_sub(old).saturating_add(new);
    Ok(())
}

/// The XFS reader as a [`Scanner`]: reads the file system that the scan root
/// is on and sends its tree where the walker's would go.
///
/// The scan root is a directory of a mounted XFS file system (the mount point
/// of a drive, usually). The reader needs administrator rights.
#[derive(Debug)]
pub struct XfsScanner {
    options: XfsOptions,
    last: Mutex<Option<XfsReport>>,
}

impl XfsScanner {
    /// A scanner with these options.
    pub fn new(options: XfsOptions) -> XfsScanner {
        XfsScanner {
            options,
            last: Mutex::new(None),
        }
    }

    /// The report of the most recent scan that produced a tree.
    pub fn last_report(&self) -> Option<XfsReport> {
        self.last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl Scanner for XfsScanner {
    fn name(&self) -> &'static str {
        "xfs-bulkstat"
    }

    fn scan(&self, root: &Path, ctx: &ScanContext) -> Result<ScanEnd, ScanError> {
        let canonical: PathBuf = std::fs::canonicalize(root).map_err(ScanError::Root)?;
        let pinned = ctx.options().pinned_root.clone();
        let inodes =
            IoctlInodes::open_pinned(&canonical, pinned.as_ref()).map_err(into_scan_error)?;
        let names = FsNames::new(canonical.clone()).pinned_to(pinned);
        let mut options = self.options.clone();
        options.max_memory_bytes = memory_cap(options.max_memory_bytes);
        let threads = options.threads.unwrap_or_else(default_threads);
        options.threads = Some(threads);
        ctx.set_threads(threads);
        match read_filesystem(
            &inodes,
            &names,
            canonical.as_os_str().as_bytes(),
            &options,
            ctx,
        ) {
            Ok((end, report)) => {
                *self
                    .last
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(report);
                Ok(end)
            }
            Err(e) => Err(into_scan_error(e)),
        }
    }
}

fn into_scan_error(e: XfsError) -> ScanError {
    match e {
        XfsError::Io(e) => ScanError::Root(e),
        XfsError::RootMoved { pinned, found } => ScanError::RootMoved { pinned, found },
        other => ScanError::Root(io::Error::new(io::ErrorKind::InvalidData, other)),
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::cast_possible_truncation,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects
    )]
    use super::*;

    #[test]
    fn mem_available_is_read_from_meminfo() {
        let text = "MemTotal:       131072000 kB\nMemFree:  1 kB\nMemAvailable:   65536 kB\nBuffers: 3 kB\n";
        assert_eq!(mem_available(text), Some(65536 * 1024));
        assert_eq!(mem_available("MemTotal: 1 kB\n"), None);
        assert_eq!(mem_available("MemAvailable: lots kB\n"), None);
        assert_eq!(mem_available("MemAvailable: 5 MB\n"), None);
        assert_eq!(
            mem_available("MemAvailable: 18446744073709551615 kB\n"),
            None
        );
    }

    #[test]
    fn the_cap_is_the_lower_of_the_ceiling_and_half_of_what_is_available() {
        let gib = 1u64 << 30;
        assert_eq!(cap_from(4 * gib, Some(100 * gib)), 4 * gib);
        assert_eq!(cap_from(4 * gib, Some(6 * gib)), 3 * gib);
        assert_eq!(cap_from(4 * gib, Some(0)), 0);
        assert_eq!(cap_from(4 * gib, None), 4 * gib);
        assert!(memory_cap(MEMORY_CEILING) <= MEMORY_CEILING);
    }

    #[test]
    fn a_directory_that_is_not_on_xfs_is_refused() {
        // The build directory is not on XFS here; on a host where it is the
        // test has nothing to check.
        let dir = std::env::temp_dir();
        match IoctlInodes::open(&dir) {
            Err(XfsError::NotXfs) | Err(XfsError::Io(_)) | Ok(_) => {}
            Err(other) => panic!("{other}"),
        }
        let missing = dir.join("no-such-directory-for-the-xfs-reader");
        assert!(matches!(IoctlInodes::open(&missing), Err(XfsError::Io(_))));
    }

    #[test]
    fn the_inode_table_stops_at_its_limit() {
        let mut rows = Vec::new();
        let mut charged = 0u64;
        let size = std::mem::size_of::<Stat>() as u64;
        let limit = 5000 * size;
        let mut n = 0u64;
        let result = loop {
            match make_room(&mut rows, &mut charged, limit) {
                Ok(()) => {
                    rows.push(Stat {
                        ino: n,
                        size: 0,
                        allocated: 0,
                        mtime: 0,
                        nlink: 1,
                        mode: 0,
                    });
                    n += 1;
                }
                Err(e) => break e,
            }
        };
        assert!(matches!(result, XfsError::TooLarge(_)));
        assert!(n > 1000 && n <= 5000, "{n}");
        assert!(charged <= limit);
    }
}
