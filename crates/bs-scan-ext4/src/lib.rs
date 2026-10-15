//! The read-only ext4 reader, used by the elevated helper on Linux. The crate
//! is empty on other systems.
//!
//! It reads a block device or an image file directly, without mounting it, and
//! produces the same batches as the directory walker: sizes, allocated sizes,
//! hard links counted once, names, modification times and categories. It
//! parses the superblock, group descriptors (also 64-bit, `flex_bg` and
//! `meta_bg`), inode tables, extent trees, classic block maps, linear and
//! indexed directories and inline-data directories. It never opens a file
//! or follows a link.
//!
//! # Distrust
//!
//! Everything on the disk is hostile, and on a mounted file system it is also
//! changing under the reader. Every length, offset, count and block number is
//! checked before it is used; all arithmetic on them is checked; nothing is
//! allocated at a size a disk field names without a cap derived from the
//! device; trees are walked with explicit stacks, extent trees with a node
//! set that stops loops, and directories with a visited set. A structure that
//! does not parse marks its own directory unreadable and becomes a counted,
//! typed problem in the [`Ext4Report`]; the rest of the file system is read.
//! A directory entry that names an inode which is free, or of another type,
//! is skipped, because a live file system produces those.
//!
//! The reader never panics on disk data: the panicking shortcuts are denied
//! for the whole crate, and the tests run it over thousands of corrupted
//! images.
//!
//! # What it does not do
//!
//! * It does not replay the journal. Metadata that a mounted file system has
//!   committed to its journal but not yet written in place is seen through the
//!   device's page cache, which the kernel keeps current; an unmounted image
//!   that needs recovery is read as it lies ([`Ext4Report::needs_recovery`]).
//! * It does not verify metadata checksums; it counts the structures it
//!   trusted without them ([`Ext4Report::unverified_blocks`]).
//! * It does not count the journal and other reserved inodes in the tree;
//!   [`Ext4Report::reserved`] says how many bytes they hold, and
//!   [`Ext4Report::unreachable`] what in-use inodes no directory reaches.
#![cfg(target_os = "linux")]
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

mod blocks;
mod dir;
mod emit;
mod error;
mod gd;
mod inode;
mod le;
mod reader;
mod sb;
mod source;

#[cfg(test)]
mod proptests;
#[cfg(test)]
mod testfs;

pub use error::{Diagnostics, Ext4Error, MAX_SAMPLES, Problem, ProblemKind};
pub use source::{ByteSource, FileSource};

use bs_scan::{ScanContext, ScanEnd, ScanError, Scanner};
use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// What a caller decides about a read.
#[derive(Clone, Debug)]
pub struct Ext4Options {
    /// Directories that are mount points of other file systems, as paths
    /// below this file system's root, separated by `/` (`mnt/usb`, no leading
    /// slash needed). They are reported empty and flagged as not followed,
    /// as the walker reports them, and what this file system holds beneath
    /// them is counted in [`Ext4Report::hidden_under_mounts`] instead. List
    /// every mount point below the file system, whatever file system is
    /// mounted there: the reader cannot tell which of them the user would see
    /// into, and what lies under a mount point is hidden from the user.
    pub skip: Vec<Vec<u8>>,
    /// The mountinfo `root` field of the mount being scanned: the directory
    /// of the file system that the mount point shows. Anything but `/` means
    /// the mount shows only part of the file system, and the read is refused
    /// with [`Ext4Error::PartialMount`]. `None` says the caller did not look,
    /// and the mount is taken to show the whole file system.
    pub mount_root: Option<Vec<u8>>,
    /// The most memory the in-memory tables may take. A file system that
    /// needs more is refused with [`Ext4Error::TooLarge`] before it can
    /// exhaust the machine; the caller falls back to the walker.
    /// [`Ext4Scanner`] lowers it to half of the memory available when the scan
    /// starts (see [`memory_cap`]); the figure used is in
    /// [`Ext4Report::memory_cap_bytes`].
    pub max_memory_bytes: u64,
    /// Threads that read directory blocks. Directories are many small reads
    /// spread over the device, which solid-state storage serves faster the
    /// more are in flight, and a spinning disk does not. `None` means one
    /// thread for [`read_filesystem`], and a number chosen from the kind of
    /// device for [`Ext4Scanner`].
    pub threads: Option<usize>,
}

/// The most memory the reader's tables may take, whatever the machine has.
pub const MEMORY_CEILING: u64 = 4 << 30;

impl Default for Ext4Options {
    fn default() -> Self {
        Ext4Options {
            skip: Vec::new(),
            mount_root: None,
            max_memory_bytes: MEMORY_CEILING,
            threads: None,
        }
    }
}

impl Ext4Options {
    /// Adds the mounts in `mounts` that lie below `fs_mount_point` (where this
    /// file system is mounted) to [`Ext4Options::skip`]. Mounts elsewhere, and
    /// the mount point itself, are ignored.
    #[must_use]
    pub fn with_other_mounts<P: AsRef<Path>>(
        mut self,
        fs_mount_point: &Path,
        mounts: impl IntoIterator<Item = P>,
    ) -> Self {
        for mount in mounts {
            if let Ok(rest) = mount.as_ref().strip_prefix(fs_mount_point) {
                let bytes = rest.as_os_str().as_bytes();
                if !bytes.is_empty() {
                    self.skip.push(bytes.to_vec());
                }
            }
        }
        self
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

/// Inodes and bytes that are on the disk but not in the tree.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Unaccounted {
    /// How many inodes.
    pub inodes: u64,
    /// Their allocated size in bytes.
    pub allocated: u64,
}

/// What lies under mount points of other file systems.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Hidden {
    /// Directories, mount points included.
    pub dirs: u64,
    /// Non-directory inodes (each once, however many names).
    pub files: u64,
    /// Their allocated size in bytes.
    pub allocated: u64,
    /// Their logical size in bytes.
    pub logical: u64,
}

/// What a read found besides the tree.
#[derive(Clone, Debug, Default)]
pub struct Ext4Report {
    /// The file system's block size.
    pub block_size: u32,
    /// How many block groups.
    pub block_groups: u32,
    /// The superblock says the file system was not cleanly unmounted and has
    /// a journal to replay. Metadata may be older than the last writes.
    pub needs_recovery: bool,
    /// The file system keeps metadata checksums.
    pub metadata_csum: bool,
    /// Blocks and inode records parsed without checking their checksums.
    pub unverified_blocks: u64,
    /// Inodes marked in use.
    pub inodes_in_use: u64,
    /// In-use inodes that no directory reaches (files deleted while open,
    /// orphans, damage). Not in the tree.
    pub unreachable: Unaccounted,
    /// Reserved inodes other than the root: the journal, quota files and the
    /// resize inode. Not in the tree.
    pub reserved: Unaccounted,
    /// What this file system holds below the mount points in
    /// [`Ext4Options::skip`].
    pub hidden_under_mounts: Hidden,
    /// The memory limit this read ran under, in bytes.
    pub memory_cap_bytes: u64,
    /// Entries or directories reported unreadable. This includes a directory
    /// marked unreadable because one of its entries was dropped.
    pub unreadable_entries: u64,
    /// Entries that named an inode which was free.
    pub vanished_entries: u64,
    /// Blocks of directories and their maps that were read.
    pub blocks_read: u64,
    /// Bytes of inode tables that were read.
    pub inode_bytes_read: u64,
    /// Every inconsistency met.
    pub diagnostics: Diagnostics,
    /// The read stopped early because it was cancelled.
    pub cancelled: bool,
    /// Time spent reading the device into memory (inode tables and
    /// directory blocks).
    pub load_time: Duration,
    /// Time spent turning that into batches and handing them on.
    pub emit_time: Duration,
}

/// Reads the ext4 file system in `src` and sends its tree to `ctx` as
/// batches. The first record is the root, named `root_name`.
///
/// `Err` only when there is no tree to send: not ext4, an unusable
/// superblock or root, or more metadata than the memory limit allows.
/// Anything less is reported in the [`Ext4Report`] and marked in the tree.
pub fn read_filesystem<S: ByteSource + ?Sized>(
    src: &S,
    root_name: &[u8],
    options: &Ext4Options,
    ctx: &ScanContext,
) -> Result<(ScanEnd, Ext4Report), Ext4Error> {
    if options.mount_root.as_deref().is_some_and(|r| r != b"/") {
        return Err(Ext4Error::PartialMount);
    }
    let geo = sb::read_geometry(src)?;
    let mut report = Ext4Report {
        memory_cap_bytes: options.max_memory_bytes,
        block_size: geo.block_size,
        block_groups: geo.groups,
        needs_recovery: geo.needs_recovery(),
        metadata_csum: geo.has_metadata_csum(),
        ..Ext4Report::default()
    };
    let metadata_csum = geo.has_metadata_csum();
    let volume = geo
        .uuid
        .as_chunks::<8>()
        .0
        .iter()
        .fold(0u64, |acc, c| acc ^ u64::from_le_bytes(*c));
    let limits = reader::Limits {
        max_memory_bytes: options.max_memory_bytes,
        threads: options.threads.unwrap_or(1).max(1),
    };
    let mut diag = Diagnostics::default();
    let begun = Instant::now();
    let loaded = reader::load(src, geo, limits, &|| ctx.is_cancelled(), &mut diag)?;
    report.load_time = begun.elapsed();
    let Some(model) = loaded else {
        report.cancelled = true;
        report.diagnostics = diag;
        return Ok((ScanEnd::Cancelled, report));
    };
    let begun = Instant::now();
    let skip = emit::SkipSet::new(&options.skip);
    let mut emitter = emit::Emitter::new(&model, ctx, skip, volume)?;
    let end = emitter.run(root_name)?;
    let (unreachable, reserved, in_use) = emitter.account_for_the_rest();
    report.emit_time = begun.elapsed();

    report.inodes_in_use = in_use;
    report.unreachable = unreachable;
    report.reserved = reserved;
    report.hidden_under_mounts = emitter.hidden;
    report.unreadable_entries = emitter.unreadable_entries;
    report.vanished_entries = emitter.vanished_entries;
    report.cancelled = emitter.cancelled;
    report.blocks_read = model.blocks_read;
    report.inode_bytes_read = model.inode_bytes_read;
    if metadata_csum {
        report.unverified_blocks = model.blocks_read.saturating_add(in_use);
    }
    diag.absorb(&emitter.diag);
    report.diagnostics = diag;
    Ok((end, report))
}

/// The ext4 reader as a [`Scanner`]: reads the device or image at `device`
/// and sends its tree where the walker's would go.
///
/// The scan's root path only names the root node and anchors the category
/// lookups, which look at paths: pass the canonical path of the mount point
/// of the file system, as the walker would be given. Unlike the walker, the
/// reader always covers the whole file system; run it for a drive's root.
#[derive(Debug)]
pub struct Ext4Scanner {
    device: PathBuf,
    options: Ext4Options,
    last: Mutex<Option<Ext4Report>>,
}

impl Ext4Scanner {
    /// A scanner for the file system on `device`.
    pub fn new(device: impl Into<PathBuf>, options: Ext4Options) -> Ext4Scanner {
        Ext4Scanner {
            device: device.into(),
            options,
            last: Mutex::new(None),
        }
    }

    /// The report of the most recent scan that produced a tree.
    pub fn last_report(&self) -> Option<Ext4Report> {
        self.last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// Threads for reading directory blocks from `device`: two on a spinning disk,
/// which only seeks more with many requests in flight, and sixteen on flash or
/// when the kind of device is unknown. The threads wait for the device rather
/// than compute, so the number of cores does not limit them; on flash, cold
/// reads kept getting faster up to about this many (measured).
fn auto_threads(device: &Path) -> usize {
    match rotational(device) {
        Some(true) => 2,
        _ => 16,
    }
}

/// Whether the disk behind `device` (a block device node, or a file on one)
/// spins, from sysfs.
fn rotational(device: &Path) -> Option<bool> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let meta = std::fs::metadata(device).ok()?;
    let number = if meta.file_type().is_block_device() {
        meta.rdev()
    } else {
        meta.dev()
    };
    // The kernel's encoding of device numbers in `st_dev`.
    let major = ((number >> 8) & 0xfff) | ((number >> 32) & 0xffff_f000);
    let minor = (number & 0xff) | ((number >> 12) & 0xffff_ff00);
    rotational_in(Path::new("/sys/dev/block"), major, minor)
}

/// [`rotational`] for the block device `major:minor`, looked up below
/// `sys_dev_block` (`/sys/dev/block` on a real system).
fn rotational_in(sys_dev_block: &Path, major: u64, minor: u64) -> Option<bool> {
    let node = sys_dev_block.join(format!("{major}:{minor}"));
    // A whole disk has its queue here; a partition has none, and its disk is
    // the parent. A missing file means "try the next place", not "unknown".
    for file in ["queue/rotational", "../queue/rotational"] {
        if let Ok(text) = std::fs::read_to_string(node.join(file)) {
            match text.trim() {
                "1" => return Some(true),
                "0" => return Some(false),
                _ => {}
            }
        }
    }
    None
}

/// Holds the device that was opened to the pinned root's file system.
fn check_device_pin(pin: Option<&bs_scan::RootPin>, src: &FileSource) -> Result<(), ScanError> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    if pin.is_none() {
        return Ok(());
    }
    let meta = src.file().metadata().map_err(ScanError::Root)?;
    bs_scan::check_device_pin(pin, meta.file_type().is_block_device(), meta.rdev())
}

impl Scanner for Ext4Scanner {
    fn name(&self) -> &'static str {
        "ext4-raw"
    }

    fn scan(&self, root: &Path, ctx: &ScanContext) -> Result<ScanEnd, ScanError> {
        let src = FileSource::open(&self.device).map_err(ScanError::Root)?;
        // This reader opens a device, not a folder, so a pin on the judged
        // root is held to the device: the one opened here must be the block
        // device the judged mount's file system is on. Checked on the opened
        // file, before a byte is read from it.
        check_device_pin(ctx.options().pinned_root.as_ref(), &src)?;
        let mut options = self.options.clone();
        options.max_memory_bytes = memory_cap(options.max_memory_bytes);
        let threads = options
            .threads
            .unwrap_or_else(|| auto_threads(&self.device));
        options.threads = Some(threads);
        ctx.set_threads(threads);
        match read_filesystem(&src, OsStr::as_bytes(root.as_os_str()), &options, ctx) {
            Ok((end, report)) => {
                *self
                    .last
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(report);
                Ok(end)
            }
            Err(Ext4Error::Io(e)) => Err(ScanError::Root(e)),
            Err(e) => Err(ScanError::Root(io::Error::new(
                io::ErrorKind::InvalidData,
                e,
            ))),
        }
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
    use std::fs;
    use std::os::unix::fs::symlink;

    fn scratch(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("ext4-lib-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    /// A fake `/sys`: one disk with a queue, one partition without, and the
    /// `/sys/dev/block` links to them.
    fn fake_sys(root: &Path, rotational: &str) {
        let disk = root.join("devices/disk");
        fs::create_dir_all(disk.join("queue")).unwrap();
        fs::write(disk.join("queue/rotational"), rotational).unwrap();
        fs::create_dir_all(disk.join("part3")).unwrap();
        fs::create_dir_all(root.join("dev/block")).unwrap();
        symlink("../../devices/disk", root.join("dev/block/8:0")).unwrap();
        symlink("../../devices/disk/part3", root.join("dev/block/8:3")).unwrap();
    }

    #[test]
    fn a_partition_takes_its_disks_answer() {
        let root = scratch("part");
        fake_sys(&root, "1\n");
        let blocks = root.join("dev/block");
        assert_eq!(rotational_in(&blocks, 8, 0), Some(true), "the whole disk");
        assert_eq!(rotational_in(&blocks, 8, 3), Some(true), "its partition");
        let flash = scratch("flash");
        fake_sys(&flash, "0\n");
        assert_eq!(rotational_in(&flash.join("dev/block"), 8, 3), Some(false));
        fs::remove_dir_all(&root).unwrap();
        fs::remove_dir_all(&flash).unwrap();
    }

    #[test]
    fn unknown_devices_and_junk_answers_are_unknown() {
        let root = scratch("unknown");
        fake_sys(&root, "maybe\n");
        let blocks = root.join("dev/block");
        assert_eq!(rotational_in(&blocks, 8, 3), None, "junk");
        assert_eq!(rotational_in(&blocks, 9, 9), None, "no such device");
        assert_eq!(rotational_in(&root.join("missing"), 8, 3), None);
        fs::remove_dir_all(&root).unwrap();
    }

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
}
