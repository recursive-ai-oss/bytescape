//! The btrfs reader holds its scan root to the mount the caller judged, on
//! the descriptor it opens, and opens the root without following a link. None
//! of this needs a btrfs file system or any privilege: the pin is checked
//! before the file system type, so a directory on any other file system shows
//! the order (a wrong pin is refused as a moved root, a right one gets as far
//! as "not btrfs").
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use bs_scan::{RootKey, RootPin, ScanError, ScanOptions, rawdir, start};
use bs_scan_btrfs::{BtrfsError, BtrfsOptions, BtrfsScanner, IoctlSource};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The pin a caller would make after judging the folder `path`.
fn pin_of(path: &Path) -> RootPin {
    RootPin::from_fd(Arc::new(rawdir::open_root(path).unwrap())).unwrap()
}

/// A pin on a mount that is certainly not the one `/proc` is on (the build
/// directory's), and the key `/proc` has.
fn wrong_pin_and_proc_key() -> (RootPin, RootKey) {
    let wrong = pin_of(Path::new(env!("CARGO_TARGET_TMPDIR")));
    let here = pin_of(Path::new(PROC)).key();
    assert_ne!(
        wrong.key(),
        here,
        "the premise: /proc is a mount of its own"
    );
    (wrong, here)
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("btrfs-pin-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A directory that is certainly not btrfs and has a mount of its own.
const PROC: &str = "/proc";

#[test]
fn a_wrong_pin_is_refused_before_the_file_system_is_looked_at() {
    let (wrong, here) = wrong_pin_and_proc_key();
    match IoctlSource::open_pinned(Path::new(PROC), Some(&wrong)) {
        Err(BtrfsError::RootMoved { pinned, found }) => {
            assert_eq!((pinned, found), (wrong.key(), Some(here)));
        }
        other => panic!("expected RootMoved, got {:?}", other.map(|_| ())),
    }
}

#[test]
fn the_right_pin_goes_on_to_the_next_check() {
    let right = pin_of(Path::new(PROC));
    assert!(matches!(
        IoctlSource::open_pinned(Path::new(PROC), Some(&right)),
        Err(BtrfsError::NotBtrfs)
    ));
    // And with no pin, as before.
    assert!(matches!(
        IoctlSource::open(Path::new(PROC)),
        Err(BtrfsError::NotBtrfs)
    ));
}

#[test]
fn a_root_that_is_a_link_is_not_opened() {
    let dir = scratch("link");
    let real = dir.join("real");
    std::fs::create_dir(&real).unwrap();
    let link = dir.join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let held = pin_of(&real);
    for pin in [None, Some(&held)] {
        match IoctlSource::open_pinned(&link, pin) {
            Err(BtrfsError::Io(e)) => assert!(matches!(e.raw_os_error(), Some(40 | 20)), "{e:?}"),
            other => panic!("the link was opened: {:?}", other.map(|_| ())),
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_pinned_scan_on_another_mount_sends_nothing() {
    let (wrong, here) = wrong_pin_and_proc_key();
    let scanner = Arc::new(BtrfsScanner::new(BtrfsOptions::default()));
    let result = start(
        scanner,
        PathBuf::from(PROC),
        ScanOptions::default().pin_root(wrong.clone()),
    )
    .wait();
    assert!(matches!(
        result.stats.error,
        Some(ScanError::RootMoved { pinned, found: Some(f) }) if pinned == wrong.key() && f == here
    ));
    assert!(result.snapshot.root().is_none());
    assert_eq!(result.stats.progress.batches, 0);
}

#[test]
fn a_scan_pinned_to_the_right_mount_is_not_refused_as_moved() {
    let scanner = Arc::new(BtrfsScanner::new(BtrfsOptions::default()));
    let result = start(
        scanner,
        PathBuf::from(PROC),
        ScanOptions::default().pin_root(pin_of(Path::new(PROC))),
    )
    .wait();
    // It is not btrfs, so the scan fails, but not as a moved root.
    assert!(matches!(result.stats.error, Some(ScanError::Root(_))));
}
