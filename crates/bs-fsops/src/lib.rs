//! File operations that never follow a link: looking an item up, measuring it
//! and deleting it, without leaving the filesystem the work started on.
//!
//! Both the interface's platform services and the elevated helper delete
//! through this crate, so there is one implementation of the dangerous part. It
//! has no dependencies beyond the operating system bindings.
//!
//! # How an item is reached
//!
//! - **Linux.** The folder that holds the item is opened by its path (links in
//!   the folder's own path are followed: the caller named that place), and the
//!   item is then reached only through that descriptor with `fstatat`,
//!   `openat` with `O_NOFOLLOW`, and `unlinkat`. A name swapped for a link
//!   while the work runs is acted on as a link; a folder swapped for a link
//!   fails to open. Every entry is compared with the device and mount of the
//!   item, so another filesystem, or a bind mount, is never entered.
//! - **Windows.** Every item and folder is opened with
//!   `FILE_FLAG_OPEN_REPARSE_POINT`, its final path is compared with the path
//!   that was judged, and the delete is requested through that open handle.
//!   A junction or link is deleted as itself.
//!
//! [`Target::open`] gives the place an item really is, in the form the
//! protected-path table reads, so the caller can judge the real path and not
//! only the text it was given.

#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

mod outcome;
mod shape;
mod target;
pub mod winpath;

#[cfg(target_os = "linux")]
pub mod remove;
#[cfg(target_os = "linux")]
pub mod walk;
#[cfg(windows)]
pub mod win;
#[cfg(windows)]
pub mod winpipe;

pub use outcome::{DeleteError, OpenError, Removed};
pub use shape::{BadPath, MAX_PATH_BYTES, validate_path};
pub use target::Target;

/// What a path named when it was looked at: the volume and the file's
/// identity on it. If the path later names something with a different
/// identity, the item was replaced.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Identity {
    /// Device number on Linux; volume serial number on Windows.
    pub volume: u64,
    /// Inode on Linux; the 128-bit file ID on Windows.
    pub file: u128,
}

#[cfg(all(test, target_os = "linux"))]
mod testutil;
