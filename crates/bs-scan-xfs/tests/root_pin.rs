//! The XFS reader holds its scan root to the mount the caller judged, on
//! both descriptors it opens (the inode source and the directory walk), and
//! opens the root without following a link. None of this needs an XFS file
//! system or any privilege: the pin is checked before the file system type.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use bs_scan::{RootKey, RootPin, ScanError, ScanOptions, rawdir, start};
use bs_scan_xfs::{FsNames, IoctlInodes, Names, XfsError, XfsOptions, XfsScanner};
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
        .join(format!("xfs-pin-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

const PROC: &str = "/proc";

#[test]
fn a_wrong_pin_is_refused_by_the_inode_source_before_the_file_system_is_looked_at() {
    let (wrong, here) = wrong_pin_and_proc_key();
    match IoctlInodes::open_pinned(Path::new(PROC), Some(&wrong)) {
        Err(XfsError::RootMoved { pinned, found }) => {
            assert_eq!((pinned, found), (wrong.key(), Some(here)));
        }
        other => panic!("expected RootMoved, got {:?}", other.map(|_| ())),
    }
    // The right pin goes on to the next check, and no pin is as before.
    let right = pin_of(Path::new(PROC));
    assert!(matches!(
        IoctlInodes::open_pinned(Path::new(PROC), Some(&right)),
        Err(XfsError::NotXfs)
    ));
    assert!(matches!(
        IoctlInodes::open(Path::new(PROC)),
        Err(XfsError::NotXfs)
    ));
}

#[test]
fn the_directory_walk_checks_its_own_descriptor_too() {
    let (wrong, here) = wrong_pin_and_proc_key();
    let pinned = FsNames::new(PROC).pinned_to(Some(wrong.clone()));
    // The walk opens the root by name once more, apart from the inode source;
    // its refusal comes out as the same typed error.
    let err = pinned.root().map(|_| ()).unwrap_err();
    assert!(matches!(
        XfsError::from(err),
        XfsError::RootMoved { pinned, found: Some(f) } if pinned == wrong.key() && f == here
    ));
    // The right pin, and no pin, open it.
    assert!(
        FsNames::new(PROC)
            .pinned_to(Some(pin_of(Path::new(PROC))))
            .root()
            .is_ok()
    );
    assert!(FsNames::new(PROC).root().is_ok());
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
        match IoctlInodes::open_pinned(&link, pin) {
            Err(XfsError::Io(e)) => assert!(matches!(e.raw_os_error(), Some(40 | 20)), "{e:?}"),
            other => panic!("the link was opened: {:?}", other.map(|_| ())),
        }
    }
    assert!(FsNames::new(&link).root().is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_pinned_scan_on_another_mount_sends_nothing() {
    let (wrong, here) = wrong_pin_and_proc_key();
    let scanner = Arc::new(XfsScanner::new(XfsOptions::default()));
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
    let scanner = Arc::new(XfsScanner::new(XfsOptions::default()));
    let result = start(
        scanner,
        PathBuf::from(PROC),
        ScanOptions::default().pin_root(pin_of(Path::new(PROC))),
    )
    .wait();
    assert!(matches!(result.stats.error, Some(ScanError::Root(_))));
}

#[test]
fn a_pin_that_is_not_enforced_cannot_slip_through() {
    // The error conversion keeps every other io error as it was.
    let plain = std::io::Error::from_raw_os_error(2);
    assert!(matches!(XfsError::from(plain), XfsError::Io(_)));
}
