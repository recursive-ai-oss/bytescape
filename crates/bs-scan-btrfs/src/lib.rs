//! The btrfs reader, used by the elevated helper on Linux. The crate is empty
//! on other systems.
//!
//! It reads a mounted btrfs file system through the kernel's tree search
//! (`BTRFS_IOC_TREE_SEARCH_V2`), which hands over the file system's metadata
//! trees directly: the inode items, the directory index items and the file
//! extent items of every subvolume. It produces the same batches as the
//! directory walker (sizes, allocated sizes, hard links counted once, names,
//! modification times, categories) and, what the walker cannot know, which
//! files share disk extents with others and how many bytes deleting a file
//! would free. It never opens a file, and it never follows a link: the search
//! only reads metadata.
//!
//! # Distrust
//!
//! The search results come from the kernel, but the reader treats them, and
//! the items inside them, as hostile all the same: every header, length,
//! count and offset is checked before use; keys must strictly increase and
//! stay in the range asked for, which also bounds the number of searches; the
//! memory the tables take is charged as they grow and held to a limit the
//! caller sets; trees are walked with explicit stacks and visited sets. A
//! structure that does not parse marks its own file or directory unreadable
//! and becomes a counted, typed problem in the [`BtrfsReport`]. A subvolume
//! whose tree cannot be read to its end is dropped whole and shown as an
//! unreadable directory, never as a half-read one.
//!
//! A directory entry that names an inode which is not in the tables is
//! skipped, because a live file system produces those between two pages of
//! the search; an entry that is dropped because it contradicts the tables
//! marks its directory unreadable, so no total above it claims to be exact.
//!
//! # Subvolumes
//!
//! The scan starts at a directory of one subvolume and covers the nested
//! subvolumes below it, snapshots included: they are separate trees of the
//! same file system, and a scan of a drive crosses into them. Nothing here can
//! cross into another file system, which has no trees in this one.
//!
//! A subvolume is nested where the root tree says so, not wherever a directory
//! entry names it. A snapshot keeps the entries of the subvolumes that were
//! nested in its origin, but not the nesting: the kernel shows an empty
//! directory there, and so does the reader ([`BtrfsReport::stand_in_dirs`]).
//! Each subvolume is therefore read, and counted, once, where it is nested.
//!
//! # Mounts
//!
//! A mount hides the folder it covers, and a walk sees the mount's files
//! there. The usual layout puts `/home` in a subvolume of its own, mounted on
//! a folder of the subvolume mounted at `/`, which the trees show as an empty
//! folder. The reader therefore asks the source for the mounts below the
//! start (from the mount table, for the real source) and, using the
//! [`bs_scan::Boundary`] in the scan's options, does this at each one:
//!
//! * a mount of this file system that the boundary lists is entered: its
//!   subvolume is read and stands in for the covered folder;
//! * every other mount, other file systems included, is shown as not
//!   followed, never as the contents of the folder it covers;
//! * a mount that the boundary lists and that cannot be placed is
//!   [`BtrfsError::MountNotPlaced`], so the caller falls back to the walker
//!   rather than getting a tree that looks whole and is not.
//!
//! What the covered folder holds on this file system is still on disk and
//! shows in [`BtrfsReport::unreachable`].
//!
//! # Sizes and sharing
//!
//! Every disk extent is counted once, at the first file emitted with it; a
//! file that has extents others also refer to is flagged shared and carries
//! the bytes only it refers to, which is what deleting it frees. Whether
//! other files refer to an extent is judged from all subvolumes of the file
//! system (the census), not only those in the scan. When that cannot be done
//! (the census is switched off, a subvolume cannot be read, or an extent item
//! does not parse and names nothing readable) every extent is taken as
//! shared, so exclusivity is never claimed on less than full evidence. A
//! compressed file reports its logical size and, as allocated size, what its
//! extents take on disk.
//!
//! # Where it differs from the walker
//!
//! The reader and the walker agree on names, types, logical sizes, times and
//! hard links. They differ, on purpose and in these ways only, on allocated
//! size:
//!
//! * **Shared extents.** The walker adds up what every file refers to, so an
//!   extent used by two files is counted twice (as `du` does); the reader
//!   counts it once, at the file it emits first, and flags both files shared.
//! * **Data kept inside the tree** (small files, link targets). The kernel
//!   reports a whole sector for them; the reader counts the bytes stored.
//! * **An extent only partly used.** After an overwrite in the middle of an
//!   extent the whole extent stays on disk. The reader counts it whole; the
//!   kernel counts the bytes the file refers to.
//! * **Compressed data.** The kernel counts a file's extents at their length
//!   in the file, compressed or not, so a compressed file shows its
//!   uncompressed size. The reader counts what the extents take on disk.
//!
//! And on one time: the empty folder shown where a snapshot holds the entry
//! of a subvolume that is not nested in it has no time on disk (the kernel
//! stamps it with the moment it is looked up), so the reader reports none.
//!
//! # What it does not do
//!
//! * It reads what the kernel hands out of the trees as they are: changes of
//!   the running transaction that are not yet in the trees are seen as the
//!   search sees them, and a file changing during the read can be seen torn
//!   between its extents and its inode.
//! * It does not read `XATTR` or `INODE_REF` items; names come from the
//!   directory index items alone.
//! * It does not use the extent tree's reference counts, which do not
//!   account for subvolumes that share metadata blocks; sharing is derived
//!   from the file extent items of every subvolume.
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

mod emit;
mod error;
mod ioctl;
mod items;
mod le;
mod model;
mod mounts;
mod search;

#[cfg(test)]
mod proptests;

pub use error::{BtrfsError, Diagnostics, MAX_SAMPLES, Problem, ProblemKind};
pub use ioctl::IoctlSource;
pub use model::Start;
pub use mounts::{MAX_MOUNTS, MountPoint, Mounted};
pub use search::{Flow, Item, Key, TreeSource};

/// Item types and ids a [`TreeSource`] implementation needs to know.
pub mod consts {
    pub use crate::items::{
        DIR_INDEX, EXTENT_DATA, FIRST_FREE, FS_TREE_ID, INODE_ITEM, LAST_FREE, ROOT_ITEM, ROOT_REF,
        ROOT_TREE_ID,
    };
}

use bs_scan::{ScanContext, ScanEnd, ScanError, Scanner};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The most memory the reader's tables may take, whatever the machine has.
pub const MEMORY_CEILING: u64 = 4 << 30;

/// What a caller decides about a read.
#[derive(Clone, Debug)]
pub struct BtrfsOptions {
    /// The most memory the in-memory tables may take. A file system that
    /// needs more is refused with [`BtrfsError::TooLarge`] before it can
    /// exhaust the machine; the caller falls back to the walker.
    /// [`BtrfsScanner`] lowers it to half of the memory available when the
    /// scan starts (see [`memory_cap`]); the figure used is in
    /// [`BtrfsReport::memory_cap_bytes`].
    pub max_memory_bytes: u64,
    /// Whether to read the subvolumes outside the scan to find out whether
    /// they use the extents inside it. On by default. Off, every extent is
    /// taken as shared, since a file shared only with a snapshot elsewhere
    /// would otherwise look exclusive and its deletion would be reported as
    /// freeing bytes it does not; only data kept inside the tree is then
    /// exclusive. Off is for measuring what the census costs. The census
    /// reads the file extent items of every other subvolume.
    pub census: bool,
}

impl Default for BtrfsOptions {
    fn default() -> Self {
        BtrfsOptions {
            max_memory_bytes: MEMORY_CEILING,
            census: true,
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

/// Inodes and bytes that are in the trees but not in the scan's tree.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Unaccounted {
    /// How many inodes.
    pub inodes: u64,
    /// The size on disk of their extents that no file in the tree counted.
    pub allocated: u64,
}

/// What is shared with other files.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sharing {
    /// Files (inodes) with at least one extent that something else also
    /// refers to.
    pub files: u64,
    /// The allocated size counted at those files.
    pub allocated: u64,
    /// Of that, the bytes only they refer to: what deleting all of them, each
    /// by itself, would free.
    pub exclusive: u64,
}

/// What compression saved, for the files that have a compressed extent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Compression {
    /// Files with at least one compressed extent.
    pub files: u64,
    /// Their logical size.
    pub logical: u64,
    /// The size on disk of their extents.
    pub on_disk: u64,
}

/// What a read found besides the tree.
#[derive(Clone, Debug, Default)]
pub struct BtrfsReport {
    /// Subvolumes read, the start's included.
    pub trees: u64,
    /// Nested subvolumes dropped because their tree could not be read.
    pub dropped_trees: u64,
    /// Other subvolumes of the file system read to find who shares extents.
    pub census_trees: u64,
    /// Mounts of this file system that the scan entered, each read from its
    /// own subvolume at the folder it covers.
    pub mounts_entered: u64,
    /// Mounts the scan did not enter (other file systems, or mounts the
    /// caller leaves out); their folders are shown as not followed.
    pub mounts_not_followed: u64,
    /// Whether the shared and exclusive figures were judged from every
    /// subvolume of the file system. `false` when the census was switched
    /// off, a subvolume could not be read or an extent item was unreadable;
    /// every extent is then taken as shared.
    pub sharing_exact: bool,
    /// Inodes read.
    pub inodes: u64,
    /// Inodes no directory of the scan reaches.
    pub unreachable: Unaccounted,
    /// Files that share extents, and their figures.
    pub sharing: Sharing,
    /// Compressed files.
    pub compression: Compression,
    /// The memory limit this read ran under, in bytes.
    pub memory_cap_bytes: u64,
    /// The memory the tables were charged, in bytes.
    pub memory_used_bytes: u64,
    /// Entries or directories reported unreadable. This includes a directory
    /// marked unreadable because one of its entries was dropped.
    pub unreadable_entries: u64,
    /// Entries that named an inode that was not there.
    pub vanished_entries: u64,
    /// Empty directories shown where a snapshot holds the entry of a
    /// subvolume that is not nested in it (the kernel shows an empty folder
    /// there too).
    pub stand_in_dirs: u64,
    /// Every inconsistency met.
    pub diagnostics: Diagnostics,
    /// The read stopped early because it was cancelled.
    pub cancelled: bool,
    /// Time spent searching the trees.
    pub load_time: Duration,
    /// Time spent turning that into batches and handing them on.
    pub emit_time: Duration,
}

/// Reads the file system `src` searches and sends the tree below `start` to
/// `ctx` as batches. The first record is the start directory, named
/// `root_name`.
///
/// `Err` only when there is no tree to send: the search failed outright, the
/// start is not a directory of its tree, or more metadata than the memory
/// limit allows. Anything less is reported in the [`BtrfsReport`] and marked
/// in the tree.
pub fn read_filesystem(
    src: &dyn TreeSource,
    start: Start,
    root_name: &[u8],
    options: &BtrfsOptions,
    ctx: &ScanContext,
) -> Result<(ScanEnd, BtrfsReport), BtrfsError> {
    let mut report = BtrfsReport {
        memory_cap_bytes: options.max_memory_bytes,
        ..BtrfsReport::default()
    };
    let limits = model::Limits {
        max_memory_bytes: options.max_memory_bytes,
        census: options.census,
    };
    let mut diag = Diagnostics::default();
    let begun = Instant::now();
    // What the scan crosses into is decided before anything is read: a mount
    // that belongs to the drive and cannot be placed ends the read here.
    let boundary = &ctx.options().boundary;
    let mount_list = src.mounts()?;
    let mounted_trees = mounts::trees_to_read(&mount_list, boundary)?;
    let loaded = model::load(
        src,
        start,
        &mounted_trees,
        limits,
        &|| ctx.is_cancelled(),
        &mut diag,
    )?;
    report.load_time = begun.elapsed();
    let Some(model) = loaded else {
        report.cancelled = true;
        report.diagnostics = diag;
        return Ok((ScanEnd::Cancelled, report));
    };
    let start_at = model.find_inode(0, start.inode).ok_or(BtrfsError::BadRoot(
        "the start directory is not in the tree",
    ))?;
    let grafts = mounts::place(&model, start_at, &mount_list, boundary, &mut diag)?;
    let begun = Instant::now();
    let mut emitter = emit::Emitter::new(&model, &grafts, ctx, options.max_memory_bytes)?;
    let end = emitter.run(start, root_name)?;
    let (unreachable, inodes) = emitter.account_for_the_rest();
    report.emit_time = begun.elapsed();

    report.trees = model.trees.iter().filter(|t| t.ok).count() as u64;
    report.dropped_trees = model.dropped_trees;
    report.mounts_entered = grafts.entered;
    report.mounts_not_followed = grafts.not_followed;
    report.census_trees = model.census_trees;
    report.sharing_exact = options.census && !model.sharing_unknown;
    report.inodes = inodes;
    report.unreachable = unreachable;
    report.sharing = emitter.sharing;
    report.compression = emitter.compression;
    report.memory_used_bytes = model.bytes();
    report.unreadable_entries = emitter.unreadable_entries;
    report.vanished_entries = emitter.vanished_entries;
    report.stand_in_dirs = emitter.stand_ins;
    report.cancelled = emitter.cancelled;
    diag.absorb(&emitter.diag);
    report.diagnostics = diag;
    Ok((end, report))
}

/// The btrfs reader as a [`Scanner`]: searches the file system that the scan
/// root is on and sends its tree where the walker's would go.
///
/// The scan root is a directory of a mounted btrfs file system (the mount
/// point of a drive, usually). The reader needs administrator rights.
#[derive(Debug)]
pub struct BtrfsScanner {
    options: BtrfsOptions,
    last: Mutex<Option<BtrfsReport>>,
}

impl BtrfsScanner {
    /// A scanner with these options.
    pub fn new(options: BtrfsOptions) -> BtrfsScanner {
        BtrfsScanner {
            options,
            last: Mutex::new(None),
        }
    }

    /// The report of the most recent scan that produced a tree.
    pub fn last_report(&self) -> Option<BtrfsReport> {
        self.last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl Scanner for BtrfsScanner {
    fn name(&self) -> &'static str {
        "btrfs-tree-search"
    }

    fn scan(&self, root: &Path, ctx: &ScanContext) -> Result<ScanEnd, ScanError> {
        let canonical = std::fs::canonicalize(root).map_err(ScanError::Root)?;
        let (src, start) = IoctlSource::open(&canonical).map_err(into_scan_error)?;
        let mut options = self.options.clone();
        options.max_memory_bytes = memory_cap(options.max_memory_bytes);
        ctx.set_threads(1);
        match read_filesystem(&src, start, canonical.as_os_str().as_bytes(), &options, ctx) {
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

fn into_scan_error(e: BtrfsError) -> ScanError {
    match e {
        BtrfsError::Io(e) => ScanError::Root(e),
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
    fn a_directory_that_is_not_on_btrfs_is_refused() {
        // The build directory is not on btrfs here; on a host where it is
        // the test has nothing to check.
        let dir = std::env::temp_dir();
        match IoctlSource::open(&dir) {
            Err(BtrfsError::NotBtrfs) => {}
            Err(BtrfsError::Io(_)) | Ok(_) => {}
            Err(other) => panic!("{other}"),
        }
        let missing = dir.join("no-such-directory-for-the-btrfs-reader");
        assert!(matches!(
            IoctlSource::open(&missing),
            Err(BtrfsError::Io(_))
        ));
    }
}
