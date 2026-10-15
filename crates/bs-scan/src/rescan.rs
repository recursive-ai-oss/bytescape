//! Rescanning one folder of a finished tree.
//!
//! [`start_rescan`] hands the tree's [`Aggregator`] to the same two-thread
//! machinery as [`crate::start`], with the scanner rooted at the folder. The
//! folder keeps its node id, name and place in the tree; its old contents
//! are replaced by what the scanner finds, held to the same limits as any scan:
//! the caller's [`ScanOptions::boundary`], no links followed, no
//! placeholders opened.
//!
//! # Threading
//!
//! * Call [`start_rescan`] from the thread that owns the aggregator (the one
//!   that got it from [`ScanResult::aggregator`]). The aggregator moves into
//!   the scan, so until the scan ends nobody can mark or unmark a deletion:
//!   the interface keeps actions off while [`ScanHandle::is_done`] is false.
//! * The interface thread keeps reading [`ScanHandle::reader`], or the
//!   [`SnapshotReader`](bs_tree::SnapshotReader) it already holds (it is the
//!   same cell): snapshots published while the rescan runs show the folder
//!   and the folders above it as incomplete, and the rest as it was.
//! * The scan ends when [`ScanHandle::is_done`] turns true. [`ScanHandle::wait`]
//!   then returns at once with the [`ScanResult`], whose aggregator is
//!   finished again and holds the tree; its `rescan_report()` says what
//!   happened.
//!
//! A view that shows anything inside the folder must be laid out again after
//! the first snapshot that has the folder incomplete: the nodes that were
//! inside are cut off ([`NodeRef::is_detached`](bs_tree::NodeRef::is_detached)).

use crate::scanner::{ScanHandle, ScanOptions, Scanner, launch};
use bs_tree::{Aggregator, NodeId, RescanError, Snapshot};
use std::path::PathBuf;
use std::sync::Arc;

/// The separator the walker of this platform writes between path parts.
#[cfg(windows)]
const SEPARATOR: u8 = b'\\';
#[cfg(not(windows))]
const SEPARATOR: u8 = b'/';

/// The path of `id` in `snapshot` as the platform names it: the root's own
/// name (the path that was scanned) followed by the names between. `None` for
/// unknown ids and for nodes that a rescan cut off.
///
/// Names are the scanner's raw bytes (WTF-8 on Windows), so the path is exact
/// whatever the characters in it. The tree never follows a link, so the path
/// is a path of real folders as of the scan; whether it still is one is for
/// the scan to find out.
pub fn node_path(snapshot: &Snapshot, id: NodeId) -> Option<PathBuf> {
    let node = snapshot.node(id)?;
    if node.is_detached() {
        return None;
    }
    let bytes = snapshot.path(id, SEPARATOR)?;
    Some(path_from_bytes(&bytes))
}

#[cfg(unix)]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(std::ffi::OsString::from_vec(bytes.to_vec()))
}

#[cfg(windows)]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    use std::os::windows::ffi::OsStringExt;
    PathBuf::from(std::ffi::OsString::from_wide(
        &crate::wininfo::utf16_from_wtf8(bytes),
    ))
}

#[cfg(not(any(unix, windows)))]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

/// A rescan that could not begin, with the aggregator it was given.
pub struct RescanRefused {
    /// Why.
    pub error: RescanError,
    /// The aggregator, unchanged.
    pub aggregator: Aggregator,
}

impl std::fmt::Debug for RescanRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RescanRefused")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Display for RescanRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

impl std::error::Error for RescanRefused {}

/// Starts replacing the contents of directory `node` of the finished tree in
/// `aggregator` with a fresh scan of `root` by `scanner`.
///
/// `root` must be the folder's path as [`node_path`] gives it. The scanner's
/// root record has to carry exactly that name (the walkers write the path
/// they were given, with links resolved once at the root); if a part of the
/// path has become a link since, the scan's root names another folder, the
/// whole scan is ignored and the tree stays as it was
/// (`rescan_report().refused`). Nothing outside the folder is ever read.
///
/// The tree is replaced when the folder itself has been read: a scan that
/// cannot open it ends with [`ScanStats::error`](crate::ScanStats) set and
/// the tree unchanged. One that fails after that leaves the folder
/// unreadable (with the error) and incomplete, and the folders above it
/// incomplete, because what is inside is not known.
///
/// `options.os_used_bytes` replaces the volume's figure when set.
///
/// # Errors
///
/// The aggregator comes back, inside the error, when the rescan cannot begin:
/// the scan that built the tree is not finished, the node is not a
/// directory, was cut off by an earlier rescan, or is (or lies under) a
/// folder marked deleted.
pub fn start_rescan(
    mut aggregator: Aggregator,
    node: NodeId,
    scanner: Arc<dyn Scanner>,
    root: PathBuf,
    options: ScanOptions,
) -> Result<ScanHandle, Box<RescanRefused>> {
    // The latest state, so the name the scan must carry is the tree's own.
    let snapshot = aggregator.publish();
    let expected = snapshot.path(node, SEPARATOR);
    if let Err(e) = aggregator.begin_rescan(node, expected.as_deref()) {
        return Err(Box::new(RescanRefused {
            error: e,
            aggregator,
        }));
    }
    if options.os_used_bytes.is_some() {
        aggregator.set_os_used_bytes(options.os_used_bytes);
    }
    Ok(launch(scanner, root, options, aggregator))
}
