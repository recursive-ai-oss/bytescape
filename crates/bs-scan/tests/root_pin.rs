//! A scan can be pinned to the root its caller judged, and its root is
//! opened without following a link.
//!
//! The pin is checked on the descriptor the walker actually opened, before
//! anything is read or sent; a mismatch ends the scan with
//! `ScanError::RootMoved` and an empty tree. The hook that stands for a
//! second process changing the path between resolving and opening is
//! process-wide, so it acts only on the one path its test made.
#![cfg(target_os = "linux")]

mod support;

use bs_scan::{RootKey, RootPin, ScanError, ScanOptions, ScanResult, rawdir};
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use support::*;

/// The pin a caller would make after judging the folder `path`: it opens the
/// folder and gives the descriptor to the pin.
fn pin_of(path: &Path) -> RootPin {
    RootPin::from_fd(Arc::new(rawdir::open_root(path).unwrap()))
        .expect("the kernel reports mount IDs")
}

/// The key of a folder, as a scanner computes it for the root it opened.
fn key_of(path: &Path) -> RootKey {
    pin_of(path).key()
}

/// Nothing was read or sent: no tree, no batch, no entry counted.
fn assert_nothing_sent(result: &ScanResult) {
    assert!(result.snapshot.root().is_none(), "a root was sent");
    let p = result.stats.progress;
    assert_eq!((p.batches, p.files, p.dirs), (0, 0, 0), "{p:?}");
    assert_eq!(result.stats.records_applied, 0);
}

/// The root hook is one per process: the tests that install it take turns.
static HOOK_TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn take_turn() -> std::sync::MutexGuard<'static, ()> {
    HOOK_TURN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn fixture(name: &str) -> Fixture {
    let fx = Fixture::new(name);
    fx.file("a/one", 1000);
    fx.file("a/b/two", 3000);
    fx.file("c", 10);
    fx.settle();
    fx
}

fn pinned(path: &Path) -> ScanOptions {
    ScanOptions::default().pin_root(pin_of(path))
}

#[test]
fn without_a_pin_the_scan_is_as_before() {
    let fx = fixture("pin-none");
    let result = scan(&fx.root);
    assert_clean(&result);
    assert_matches_du(&fx.root, &result);
    assert!(ScanOptions::default().pinned_root.is_none());
}

#[test]
fn a_pin_on_the_roots_own_mount_runs_unchanged() {
    let fx = fixture("pin-right");
    let plain = scan(&fx.root);
    let result = scan_with(&fx.root, pinned(&fx.root));
    assert_clean(&result);
    assert_matches_du(&fx.root, &result);
    assert!(result.stats.error.is_none());
    let (a, b) = (
        plain.snapshot.root().unwrap(),
        result.snapshot.root().unwrap(),
    );
    assert_eq!(
        (a.logical(), a.allocated(), plain.snapshot.len()),
        (b.logical(), b.allocated(), result.snapshot.len())
    );
}

#[test]
fn a_pin_on_a_real_but_different_mount_is_refused_with_nothing_sent() {
    let fx = fixture("pin-other");
    let here = key_of(&fx.root);
    // /proc is a mount of its own on any Linux system.
    let other = key_of(Path::new("/proc"));
    assert_ne!(
        here, other,
        "the premise: /proc is not on the fixture's mount"
    );
    let result = scan_with(&fx.root, pinned(Path::new("/proc")));
    match &result.stats.error {
        Some(ScanError::RootMoved {
            pinned,
            found: Some(f),
        }) => assert_eq!((*pinned, *f), (other, here)),
        other => panic!("expected RootMoved, got {other:?}"),
    }
    assert_eq!(result.stats.end, None);
    assert_nothing_sent(&result);
    result.snapshot.verify().unwrap();
    assert!(!result.snapshot.scan_complete());
    // And the other way round: a scan of /proc pinned to the fixture's mount.
    let result = scan_with(Path::new("/proc/self/task"), pinned(&fx.root));
    assert!(matches!(
        result.stats.error,
        Some(ScanError::RootMoved { .. })
    ));
    assert_nothing_sent(&result);
}

#[test]
fn a_pin_is_a_mount_not_a_folder() {
    // The verdict is about the mount: another folder of the same mount passes.
    let fx = fixture("pin-same-mount");
    let result = scan_with(&fx.path("a"), pinned(&fx.path("a/b")));
    assert_clean(&result);
}

#[test]
fn the_refusal_has_words_for_both_cases() {
    let pinned = RootKey::Mount {
        id: 5,
        unique: true,
    };
    let known = ScanError::RootMoved {
        pinned,
        found: Some(RootKey::Mount {
            id: 9,
            unique: true,
        }),
    };
    let unknown = ScanError::RootMoved {
        pinned,
        found: None,
    };
    assert!(known.to_string().contains("mount 9"));
    assert!(unknown.to_string().contains("cannot tell"));
}

#[test]
fn the_check_passes_without_a_pin_and_compares_the_descriptor_given() {
    let fx = fixture("pin-check");
    let fd = rawdir::open_root(&fx.root).unwrap();
    assert!(bs_scan::check_root_pin(None, fd.as_fd()).is_ok());
    let pin = pin_of(&fx.root);
    assert!(bs_scan::check_root_pin(Some(&pin), fd.as_fd()).is_ok());
    let proc_fd = rawdir::open_root(Path::new("/proc")).unwrap();
    assert!(matches!(
        bs_scan::check_root_pin(Some(&pin), proc_fd.as_fd()),
        Err(ScanError::RootMoved { .. })
    ));
}

#[test]
fn a_pin_keeps_the_judged_descriptor_open_and_clones_share_it() {
    let fx = fixture("pin-held");
    let held = Arc::new(rawdir::open_root(&fx.root).unwrap());
    let pin = RootPin::from_fd(Arc::clone(&held)).unwrap();
    let again = pin.clone();
    drop(held);
    drop(pin);
    // The clone still works: the descriptor it holds is open.
    let result = scan_with(&fx.root, ScanOptions::default().pin_root(again));
    assert_clean(&result);
}

#[test]
fn the_pins_device_is_the_one_the_file_system_reports() {
    let fx = fixture("pin-device");
    let pin = pin_of(&fx.root);
    let st = rawdir::stat_fd(rawdir::open_root(&fx.root).unwrap().as_fd()).unwrap();
    // The walker's `dev` is major in the high half, minor in the low.
    assert_eq!(
        pin.device(),
        Some(((st.dev >> 32) as u32, (st.dev & 0xffff_ffff) as u32))
    );
}

#[test]
fn a_device_check_wants_a_block_device_with_the_pins_number() {
    let fx = fixture("pin-dev-check");
    let pin = pin_of(&fx.root);
    let (major, minor) = pin.device().unwrap();
    let same = libc::makedev(major, minor);
    let other = libc::makedev(major, minor.wrapping_add(1));
    // No pin: always fine.
    assert!(bs_scan::check_device_pin(None, false, 0).is_ok());
    // The right number, a block device: fine.
    assert!(bs_scan::check_device_pin(Some(&pin), true, same).is_ok());
    // The right number on something that is not a block device (an image
    // file, a character device): refused, found is unknown.
    assert!(matches!(
        bs_scan::check_device_pin(Some(&pin), false, same),
        Err(ScanError::RootMoved { found: None, .. })
    ));
    // Another device: refused, and the other one is named.
    match bs_scan::check_device_pin(Some(&pin), true, other) {
        Err(ScanError::RootMoved {
            pinned,
            found: Some(found),
        }) => {
            assert_eq!(pinned, RootKey::Device { major, minor });
            assert_eq!(
                found,
                RootKey::Device {
                    major,
                    minor: minor.wrapping_add(1)
                }
            );
        }
        other => panic!("expected RootMoved, got {other:?}"),
    }
}

#[test]
fn a_root_that_is_a_link_is_not_opened() {
    let fx = Fixture::new("root-link");
    let real = fx.dir("real");
    std::fs::write(real.join("f"), b"x").unwrap();
    let link = fx.path("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    // The primitive refuses a link in the last place.
    let err = rawdir::open_root(&link).unwrap_err();
    assert!(
        matches!(err.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)),
        "{err:?}"
    );
    // And still opens the folder itself.
    assert!(rawdir::open_root(&real).is_ok());
    // A scan of a path that is a link is the user's choice: it is resolved
    // once, up front, and the folder it names is scanned.
    let result = scan(&link);
    assert_clean(&result);
}

/// Changes `target` between its being resolved and its being opened: the
/// folder is moved aside and a link to it put in its place, as a process
/// sharing the user could do.
fn swap_for_a_link_once(target: PathBuf) {
    let aside = target.with_extension("aside");
    let done = std::sync::atomic::AtomicBool::new(false);
    bs_scan::set_before_root_open_hook(Some(Box::new(move |resolved| {
        if resolved == target && !done.swap(true, std::sync::atomic::Ordering::SeqCst) {
            std::fs::rename(&target, &aside).unwrap();
            std::os::unix::fs::symlink(&aside, &target).unwrap();
        }
    })));
}

#[test]
fn a_root_that_became_a_link_after_it_was_resolved_is_refused() {
    let _turn = take_turn();
    let fx = fixture("root-swapped");
    let root = fx.dir("scanme");
    std::fs::write(root.join("f"), vec![b'x'; 5000]).unwrap();
    let resolved = std::fs::canonicalize(&root).unwrap();
    swap_for_a_link_once(resolved);
    let result = scan(&root);
    bs_scan::set_before_root_open_hook(None);
    assert!(
        matches!(
            &result.stats.error,
            Some(ScanError::NotADirectory(_)) | Some(ScanError::Root(_))
        ),
        "the link was followed: {:?}",
        result.stats.error
    );
    assert_nothing_sent(&result);
    // The same swap with a pin: the link is still not followed.
    let root = fx.dir("scanme2");
    let resolved = std::fs::canonicalize(&root).unwrap();
    swap_for_a_link_once(resolved);
    let result = scan_with(&root, pinned(&fx.root));
    bs_scan::set_before_root_open_hook(None);
    assert!(result.stats.error.is_some());
    assert_nothing_sent(&result);
}

#[test]
fn a_hook_for_one_path_leaves_other_scans_alone() {
    let _turn = take_turn();
    let fx = fixture("hook-scope");
    let elsewhere = fx.root.join("never-this");
    bs_scan::set_before_root_open_hook(Some(Box::new(move |resolved| {
        assert_ne!(resolved, elsewhere, "the hook saw the wrong path");
    })));
    let result = scan(&fx.root);
    bs_scan::set_before_root_open_hook(None);
    assert_clean(&result);
}
