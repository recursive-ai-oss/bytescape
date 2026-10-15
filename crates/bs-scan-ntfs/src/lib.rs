//! The NTFS master file table reader, used by the elevated helper on Windows.
//!
//! It reads a volume (`\\.\C:`) or an image file directly and produces the
//! same batches as the directory walker: sizes, allocated sizes, hard links
//! counted once, names, modification times and categories. It reads the
//! table sequentially from its own run list, joins each file's attributes
//! across extension records, rebuilds directories from the parent each name
//! points at, and never opens a user file or follows a link. The parsers read
//! from a [`ByteSource`], so the whole reader builds and is tested on every
//! platform; only opening a raw volume is Windows-specific, and that uses the
//! standard library alone.
//!
//! # Distrust
//!
//! Everything on the disk is hostile, and on a mounted volume it is also
//! changing under the reader. Every length, offset, count and cluster number
//! is checked before it is used; all arithmetic on them is checked or
//! saturating; nothing is allocated at a size a disk field names without a
//! cap derived from the volume; trees are walked with explicit stacks and
//! visited sets. The memory a read takes is charged as it goes and held to a
//! limit the caller sets, because a volume can name far more than it holds.
//! A record that does not parse is skipped and counted, and the root is
//! marked unreadable, because the reader cannot tell what was in it; an
//! attribute that does not parse marks its own file. Both become counted,
//! typed problems in the [`NtfsReport`].
//!
//! A file that no directory reaches is not left out: when it has a name or
//! holds clusters it is placed in a made-up folder of the root (flagged
//! synthetic, so that nothing is ever done to it by path), which is marked
//! unreadable so that every total above it reads as a lower bound. Only
//! records with no name and no bytes (the reserved ones every volume has),
//! and what lies below a link or mount point the walk did not enter, stay
//! out, and the report counts them.
//!
//! # What is counted
//!
//! The default, [`Accounting::Full`], counts what a file occupies on the
//! volume: the clusters of every data stream (alternate streams included) and
//! of the attributes that live outside the table record (an attribute list,
//! extended attributes, a directory's index buffers). Resident data costs no
//! clusters. Compressed and sparse streams count the clusters their run
//! lists actually back. The table itself and the other system files
//! (`$MFT`, `$LogFile`, `$Bitmap`, `$Extend`…) are entries of the root, with
//! their real sizes. [`Accounting::Listing`] counts only what a directory
//! listing reports, to compare with the walker.
//!
//! The report says what the full count adds beyond the listing count
//! ([`Extras`]) and what the tree could not place ([`Unaccounted`]).
//!
//! # What it does not do
//!
//! * It does not replay the log file. A mounted volume's raw blocks can lag
//!   the file system's cache by seconds, so a live read can miss the newest
//!   changes; flush the volume first when that matters.
//! * It does not decode `$INDEX_ALLOCATION` directory indexes; the names in
//!   the file records are the authority.
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation
)]

mod boot;
mod budget;
mod emit;
mod error;
mod le;
mod mft;
mod parse;
mod record;
mod runlist;
mod source;

// The in-memory volume builder lives with the tests (`tests/common/testkit.rs`)
// and is compiled into each test program and into this crate's own tests, so
// the library an elevated program links carries none of it. Its code names
// the crate by its public name, which the line below makes work inside the
// crate too.
#[cfg(test)]
extern crate self as bs_scan_ntfs;
#[cfg(test)]
#[path = "../tests/common/testkit.rs"]
mod testkit;

#[cfg(test)]
mod proptests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_hostile;

pub use error::{Diagnostics, MAX_SAMPLES, NtfsError, Problem, ProblemKind};
pub use source::{ByteSource, FileSource};

use bs_scan::{ScanContext, ScanEnd, ScanError, Scanner};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How sizes are counted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Accounting {
    /// Everything a file occupies: all data streams and the non-resident
    /// attributes. The truthful default.
    #[default]
    Full,
    /// Only what a directory listing reports: a file's unnamed data stream,
    /// and nothing for a directory. For comparing with the walker.
    Listing,
}

/// The most memory the reader's tables may take, whatever the machine has.
pub const MEMORY_CEILING: u64 = 4 << 30;

/// The memory limit for a read that starts now: `ceiling`, or half of the
/// memory the machine reports as available if that is less. The reader may
/// run elevated with the whole machine to itself, and running it out of
/// memory would take the user's session down with it.
///
/// The reader forbids `unsafe` code and so cannot ask the system itself: the
/// caller passes what the system reports as available physical memory
/// (`ullAvailPhys` of `GlobalMemoryStatusEx` on Windows, `MemAvailable` on
/// Linux). `None`, when it cannot be read, leaves the ceiling unchanged.
pub fn memory_cap(ceiling: u64, available_bytes: Option<u64>) -> u64 {
    match available_bytes {
        Some(bytes) => ceiling.min(bytes / 2),
        None => ceiling,
    }
}

/// What a caller decides about a read.
#[derive(Clone, Debug)]
pub struct NtfsOptions {
    /// The most memory the read may take for what grows with the volume: the
    /// file table, every name and its text, the attribute lists, the
    /// directory index and the tree the scan builds. A volume that needs more
    /// is refused with [`NtfsError::TooLarge`] before it can exhaust the
    /// machine, and the caller falls back to the walker. The figure is
    /// charged as the reader goes, so a volume that names far more than it
    /// holds is refused as well; the peak overshoots it by a few chunks of
    /// the table at most.
    ///
    /// The default is [`MEMORY_CEILING`]; set it with [`memory_cap`] from the
    /// memory the system reports as available.
    pub max_memory_bytes: u64,
    /// Threads that parse the table. `None` picks one from the number of
    /// cores.
    pub threads: Option<usize>,
    /// How sizes are counted.
    pub accounting: Accounting,
    /// The separator the scan root's path uses, for the category lookups
    /// that look at paths: `\` for a drive, `/` for a path on another
    /// system.
    pub path_separator: u8,
}

impl NtfsOptions {
    /// Limits [`NtfsOptions::max_memory_bytes`] to what [`memory_cap`] allows
    /// for the memory the system reports as available.
    #[must_use]
    pub fn with_available_memory(mut self, available_bytes: Option<u64>) -> Self {
        self.max_memory_bytes = memory_cap(self.max_memory_bytes, available_bytes);
        self
    }
}

impl Default for NtfsOptions {
    fn default() -> Self {
        NtfsOptions {
            max_memory_bytes: MEMORY_CEILING,
            threads: None,
            accounting: Accounting::Full,
            path_separator: b'\\',
        }
    }
}

/// Files in use, and the bytes of clusters they hold.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Unaccounted {
    /// Files (base records in use).
    pub files: u64,
    /// Bytes of clusters they hold.
    pub allocated: u64,
}

/// What [`Accounting::Full`] counts beyond what a directory listing reports,
/// over the entries that are in the tree.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Extras {
    /// Alternate data streams.
    pub streams: u64,
    /// Their sizes.
    pub stream_logical: u64,
    /// The clusters they hold.
    pub stream_allocated: u64,
    /// Clusters held by non-resident attributes that are not data (attribute
    /// lists, index buffers, bitmaps, extended attributes).
    pub attribute_allocated: u64,
    /// The part of the last figure that belongs to directories.
    pub directory_allocated: u64,
}

/// How many of the entries in the tree the table marks as reparse points
/// (links, junctions, mount points, cloud placeholders and the system's own
/// overlays), counted by the reparse tag the reader found for them (see [`ReparseCounts::by_tag`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReparseCounts {
    /// The tags seen and how many entries carry each. The reader takes a tag
    /// only from a `$REPARSE_POINT` attribute that is stored in the file's
    /// base record. A tag of 0 therefore means the entry's attributes say it
    /// is a reparse point and the reader found no tag: it may not have one,
    /// or its attribute may be stored outside the base record (in an
    /// extension record, or in clusters), where the reader does not look.
    /// At most [`ReparseCounts::MAX_TAGS`] tags are kept apart.
    pub by_tag: Vec<(u32, u64)>,
    /// Entries whose tag did not fit in the list.
    pub other: u64,
}

impl ReparseCounts {
    /// Distinct tags kept apart. A damaged table can name any number.
    pub const MAX_TAGS: usize = 64;

    pub(crate) fn note(&mut self, tag: u32) {
        if let Some(entry) = self.by_tag.iter_mut().find(|(t, _)| *t == tag) {
            entry.1 = entry.1.saturating_add(1);
        } else if self.by_tag.len() < Self::MAX_TAGS {
            self.by_tag.push((tag, 1));
        } else {
            self.other = self.other.saturating_add(1);
        }
    }
}

/// What a read found besides the tree.
#[derive(Clone, Debug, Default)]
pub struct NtfsReport {
    /// The volume's serial number.
    pub volume_serial: u64,
    /// Bytes per cluster.
    pub cluster_bytes: u32,
    /// Bytes per file record.
    pub record_bytes: u32,
    /// Slots in the master file table.
    pub records: u64,
    /// Files in use (base records).
    pub files_in_use: u64,
    /// Records that were unreadable or damaged beyond use. Whatever they held
    /// is missing from the tree, which is why the root is then unreadable.
    pub lost_records: u64,
    /// In-use files that are not in the tree: records with no name and no
    /// bytes (the reserved records every volume has), and files below a
    /// directory that is a link or mount point and so was not entered.
    pub unreachable: Unaccounted,
    /// In-use files that no directory reaches (their directory is gone or
    /// stale), found with a name or with bytes. They are in the tree, in a
    /// folder of the root named `<files outside any folder>` that is marked
    /// unreadable, so the totals above it read as a lower bound: where they
    /// belong is not known, and a directory of theirs may be among the
    /// records the table lost. The folder is made up, not a real path, and
    /// is flagged [`bs_tree::NodeFlags::SYNTHETIC`]: its name can equal that
    /// of a real directory, so only the flag identifies it.
    pub adopted: Unaccounted,
    /// Entries of directories that are links or mount points, which are not
    /// entered.
    pub not_entered: u64,
    /// What full accounting adds beyond the listing.
    pub extras: Extras,
    /// The entries in the tree that the table marks as reparse points, by tag.
    pub reparse: ReparseCounts,
    /// Entries or directories reported unreadable.
    pub unreadable_entries: u64,
    /// Attribute lists that could not be read, so their files may be missing
    /// pieces.
    pub lists_unchecked: u64,
    /// Files that have an attribute list.
    pub attribute_lists: u64,
    /// Extension records joined to their files.
    pub extension_records: u64,
    /// Bytes of the table that were read.
    pub mft_bytes_read: u64,
    /// Every inconsistency met.
    pub diagnostics: Diagnostics,
    /// The read stopped early because it was cancelled.
    pub cancelled: bool,
    /// Time spent reading and parsing the table.
    pub load_time: Duration,
    /// Time spent building the tree and handing it on.
    pub emit_time: Duration,
}

/// Reads the NTFS volume in `src` and sends its tree to `ctx` as batches.
/// The first record is the root, named `root_name`.
///
/// `Err` only when there is no tree to send: not NTFS, an unusable boot
/// sector, table or root, or more metadata than the memory limit allows.
/// Anything less is reported in the [`NtfsReport`] and marked in the tree.
pub fn read_volume<S: ByteSource + ?Sized>(
    src: &S,
    root_name: &[u8],
    options: &NtfsOptions,
    ctx: &ScanContext,
) -> Result<(ScanEnd, NtfsReport), NtfsError> {
    let mut sector = vec![0u8; 512];
    src.read_at(0, &mut sector)?;
    let geo = boot::parse(&sector, src.len())?;
    let mut report = NtfsReport {
        volume_serial: geo.serial,
        cluster_bytes: geo.cluster_bytes,
        record_bytes: geo.record_bytes,
        ..NtfsReport::default()
    };
    let limits = mft::Limits {
        max_memory_bytes: options.max_memory_bytes,
        threads: options.threads.unwrap_or_else(default_threads).max(1),
    };
    let mut diag = Diagnostics::default();
    let begun = Instant::now();
    let loaded = mft::load(src, geo, limits, &|| ctx.is_cancelled(), &mut diag)?;
    report.load_time = begun.elapsed();
    let Some(model) = loaded else {
        report.cancelled = true;
        report.diagnostics = diag;
        return Ok((ScanEnd::Cancelled, report));
    };
    let begun = Instant::now();
    let mut emitter = emit::Emitter::new(
        &model,
        ctx,
        options.accounting,
        options.path_separator,
        options.max_memory_bytes,
        &mut diag,
    )?;
    let end = emitter.run(root_name)?;
    let (unreachable, in_use) = emitter.account_for_the_rest();
    report.emit_time = begun.elapsed();

    report.records = model.files.len() as u64;
    report.files_in_use = in_use;
    report.lost_records = model.lost;
    report.unreachable = unreachable;
    report.adopted = emitter.adopted;
    report.not_entered = emitter.not_entered;
    report.extras = emitter.extras;
    report.reparse = std::mem::take(&mut emitter.reparse);
    report.unreadable_entries = emitter.unreadable_entries;
    report.lists_unchecked = model.lists_unchecked;
    report.attribute_lists = model.lists;
    report.extension_records = model.extensions_joined;
    report.mft_bytes_read = model.mft_bytes;
    report.cancelled = emitter.cancelled;
    diag.absorb(&model.diag);
    diag.absorb(&emitter.diag);
    report.diagnostics = diag;
    Ok((end, report))
}

fn default_threads() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get().min(8))
}

/// The NTFS reader as a [`Scanner`]: reads the volume or image at `device`
/// and sends its tree where the walker's would go.
///
/// The scan's root path only names the root node and anchors the category
/// lookups, which look at paths: pass the drive's root (`C:\`) as the walker
/// would be given. Unlike the walker, the reader always covers the whole
/// volume; run it for a drive's root.
#[derive(Debug)]
pub struct NtfsScanner {
    device: PathBuf,
    options: NtfsOptions,
    last: Mutex<Option<NtfsReport>>,
}

impl NtfsScanner {
    /// A scanner for the volume at `device`: a path such as `\\.\C:`, or an
    /// image file.
    pub fn new(device: impl Into<PathBuf>, options: NtfsOptions) -> NtfsScanner {
        NtfsScanner {
            device: device.into(),
            options,
            last: Mutex::new(None),
        }
    }

    /// The report of the most recent scan that produced a tree.
    pub fn last_report(&self) -> Option<NtfsReport> {
        self.last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// A path as the bytes the tree stores for a name: UTF-8 (WTF-8 on Windows,
/// where paths are UTF-16).
fn path_bytes(path: &Path) -> Vec<u8> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let mut out = Vec::new();
        bs_scan::winrules::wtf8_from_utf16(path.as_os_str().encode_wide(), &mut out);
        out
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(any(windows, unix)))]
    {
        path.to_string_lossy().into_owned().into_bytes()
    }
}

/// Whether a path names a raw device (`\\.\C:`) rather than a file.
fn is_raw_volume(path: &Path) -> bool {
    path.to_string_lossy().starts_with(r"\\.\")
}

impl Scanner for NtfsScanner {
    fn name(&self) -> &'static str {
        "ntfs-mft"
    }

    fn scan(&self, root: &Path, ctx: &ScanContext) -> Result<ScanEnd, ScanError> {
        let src = if is_raw_volume(&self.device) {
            FileSource::open_volume(&self.device)
        } else {
            FileSource::open(&self.device)
        }
        .map_err(ScanError::Root)?;
        let mut options = self.options.clone();
        let threads = options.threads.unwrap_or_else(default_threads);
        options.threads = Some(threads);
        ctx.set_threads(threads);
        match read_volume(&src, &path_bytes(root), &options, ctx) {
            Ok((end, report)) => {
                *self
                    .last
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(report);
                Ok(end)
            }
            Err(NtfsError::Io(e)) => Err(ScanError::Root(e)),
            Err(e) => Err(ScanError::Root(io::Error::new(
                io::ErrorKind::InvalidData,
                e,
            ))),
        }
    }
}
