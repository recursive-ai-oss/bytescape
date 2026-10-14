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
pub use scanner::{
    ScanContext, ScanEnd, ScanError, ScanHandle, ScanOptions, ScanProgress, ScanResult, ScanStats,
    Scanner, start,
};
pub use volume::{FixedVolume, StatvfsVolume, VolumeInfo};

#[cfg(target_os = "linux")]
pub use linux::Walker;

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
