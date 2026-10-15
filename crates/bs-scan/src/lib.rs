//! The scanner interface and the parallel directory walker that works on every
//! platform and every filesystem.
//!
//! * [`Scanner`] is what a scan producer implements: the walker here, and the
//!   fast backends that read filesystem metadata directly. [`start`] runs one
//!   and feeds its batches to a `bs-tree` aggregator on its own thread; see the
//!   [`scanner`] module for the threading model.
//! * [`Walker`] is the one that always works: unprivileged, parallel, never
//!   crossing into another drive, never following links, never opening cloud
//!   placeholders, recording what it cannot read instead of failing.
//! * [`start_rescan`] replaces one folder of a finished tree with a fresh scan
//!   of it, keeping the folder where it is; [`node_path`] gives the folder's
//!   path.
//! * [`Boundary`] says which mounts count as the same drive.
//! * Each file gets its category (what colors it on the map) while it is read:
//!   by its extension, or by the place it lies in when the category tables
//!   give that place a category. [`ScanOptions::categories`] turns this off.
//! * [`VolumeInfo`] gives the system's used-bytes figure that progress is
//!   measured against.
//!
//! ```no_run
//! use bs_scan::{ScanOptions, Walker, VolumeInfo, StatvfsVolume};
//! use std::path::Path;
//!
//! let root = Path::new("/home");
//! let options = ScanOptions {
//!     os_used_bytes: StatvfsVolume.used_bytes(root).ok(),
//!     ..ScanOptions::default()
//! };
//! let handle = bs_scan::start(std::sync::Arc::new(Walker), root.to_path_buf(), options);
//! // Draw from `handle.reader().load()` while it runs; then:
//! let result = handle.wait();
//! assert!(result.snapshot.scan_complete() || result.stats.error.is_some());
//! ```

mod boundary;
#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
mod classify;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod dirent;
mod pool;
mod rescan;
mod scanner;
mod threads;
mod volume;
// Windows record parsers: plain byte handling, built and tested everywhere.
mod wininfo;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

pub use boundary::{Boundary, Placement};
/// What the category lookups resolve the user's places against; see
/// [`ScanOptions::environment`].
pub use bs_knowledge::Environment;
/// The category lookup the walker uses, and the path joining it expects, for
/// other producers that must give the same entries the same categories.
pub use classify::{Classifier, MAX_PATH_BYTES, join as join_path};
pub use rescan::{RescanRefused, node_path, start_rescan};
pub use scanner::{
    ScanContext, ScanEnd, ScanError, ScanHandle, ScanOptions, ScanProgress, ScanResult, ScanStats,
    Scanner, start,
};
pub use volume::{FixedVolume, StatvfsVolume, VolumeInfo};

/// The rules the Windows walker applies to a directory entry (what a reparse
/// point or cloud placeholder is, which attributes mean sparse or compressed,
/// how names and times are converted), for other producers of the same
/// entries.
pub mod winrules {
    pub use crate::wininfo::{
        Classified, EntryClass, attr, classify, entry_flags, filetime_to_unix, is_plain_component,
        utf16_from_wtf8, wtf8_from_utf16,
    };
}

#[cfg(target_os = "linux")]
pub use linux::Walker;

/// The walker with its per-entry `statx` calls going through an io_uring, for
/// the benchmark only. Not part of the product: it exists to answer whether
/// that is faster, and is compiled only with the `iouring-bench` feature.
#[cfg(all(target_os = "linux", feature = "iouring-bench"))]
#[doc(hidden)]
pub mod iouring_bench {
    pub use crate::linux::uring::{
        Config, IoUringWalker, Stats, Unavailable, probe, process_cpu_ms, set_force_ring_failure,
        set_force_unavailable, take_stats,
    };
}

/// The directory primitives the Linux walker is made of, for other producers
/// that must list directories the same way: opening a directory without
/// following links, reading its entries with `getdents64`, and asking for the
/// few fields of a directory that the walker uses. They are what keeps a
/// producer from following a link or crossing a mount by accident.
#[cfg(target_os = "linux")]
pub mod rawdir {
    pub use crate::dirent::{Entries, ParseError, RawEntry, entries};
    pub use crate::linux::sys::{
        Stat, getdents, open_dir_at, open_root, raise_open_file_limit, stat_at, stat_fd,
    };
}

/// The hook type for [`set_before_open_hook`].
#[cfg(any(target_os = "linux", windows))]
#[doc(hidden)]
pub type OpenHook = Box<dyn Fn(&[u8]) + Send + Sync>;

/// Calls `hook` with a directory's name (raw bytes on Linux, WTF-8 on
/// Windows) just before the walker opens it, so a test can swap the directory
/// for a link at that moment. For tests only; affects the whole process.
#[cfg(any(target_os = "linux", windows))]
#[doc(hidden)]
pub fn set_before_open_hook(hook: Option<OpenHook>) {
    #[cfg(target_os = "linux")]
    linux::set_before_open_hook(hook);
    #[cfg(windows)]
    windows::set_before_open_hook(hook);
}

/// Makes the Linux walker behave as on a kernel that does not report mount IDs
/// (before 5.8), so the fallback to device numbers can be tested. Affects the
/// whole process; for tests only.
#[cfg(target_os = "linux")]
#[doc(hidden)]
pub fn force_device_fallback(on: bool) {
    linux::sys::set_ignore_mount_ids(on);
}
#[cfg(windows)]
pub use windows::Walker;

/// There is no walker for this platform.
#[cfg(not(any(target_os = "linux", windows)))]
#[derive(Clone, Copy, Debug, Default)]
pub struct Walker;

#[cfg(not(any(target_os = "linux", windows)))]
impl Scanner for Walker {
    fn name(&self) -> &'static str {
        "unsupported"
    }

    fn scan(&self, _root: &std::path::Path, _ctx: &ScanContext) -> Result<ScanEnd, ScanError> {
        Err(ScanError::Unsupported)
    }
}
