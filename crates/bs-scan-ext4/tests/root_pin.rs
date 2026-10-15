//! The ext4 reader opens a device, not a folder, so a pin on the judged root
//! is held to the device: the opened file must be a block device with the
//! number of the file system the judged mount is on. A refusal comes before a
//! byte is read, and nothing is sent. Opening a real block device needs
//! privilege, so the accepting side is covered by the number logic in
//! `bs-scan` and by the live tests on a privileged runner; here the refusals
//! are exercised with files that are not block devices.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use bs_scan::{RootKey, RootPin, ScanError, ScanOptions, rawdir, start};
use bs_scan_ext4::{Ext4Options, Ext4Scanner};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("ext4-pin-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn pin_of(path: &Path) -> RootPin {
    RootPin::from_fd(Arc::new(rawdir::open_root(path).unwrap())).unwrap()
}

#[test]
fn a_pinned_scan_of_something_that_is_not_a_block_device_is_refused_unread() {
    let dir = scratch("file");
    // A file of zeros: if the reader looked inside it would fail differently
    // (as a bad superblock), so the error type shows the pin came first.
    let image = dir.join("image");
    std::fs::write(&image, vec![0u8; 1 << 16]).unwrap();
    let pin = pin_of(&dir);
    let (major, minor) = pin.device().unwrap();
    let scanner = Arc::new(Ext4Scanner::new(&image, Ext4Options::default()));
    let result = start(
        scanner,
        PathBuf::from("/"),
        ScanOptions::default().pin_root(pin),
    )
    .wait();
    assert!(
        matches!(
            result.stats.error,
            Some(ScanError::RootMoved {
                pinned: RootKey::Device { major: a, minor: b },
                found: None,
            }) if (a, b) == (major, minor)
        ),
        "{:?}",
        result.stats.error
    );
    assert!(result.snapshot.root().is_none());
    assert_eq!(result.stats.progress.batches, 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_character_device_with_the_right_number_is_still_refused() {
    // /dev/null is a device node but not a block device.
    let pin = pin_of(Path::new("/proc"));
    let scanner = Arc::new(Ext4Scanner::new("/dev/null", Ext4Options::default()));
    let result = start(
        scanner,
        PathBuf::from("/"),
        ScanOptions::default().pin_root(pin),
    )
    .wait();
    assert!(matches!(
        result.stats.error,
        Some(ScanError::RootMoved { found: None, .. })
    ));
    assert_eq!(result.stats.progress.batches, 0);
}

#[test]
fn without_a_pin_the_device_is_not_questioned() {
    let dir = scratch("nopin");
    let image = dir.join("image");
    std::fs::write(&image, vec![0u8; 1 << 16]).unwrap();
    let scanner = Arc::new(Ext4Scanner::new(&image, Ext4Options::default()));
    let result = start(scanner, PathBuf::from("/"), ScanOptions::default()).wait();
    // Not an ext4 image, so it fails, but not as a moved root.
    assert!(
        !matches!(result.stats.error, Some(ScanError::RootMoved { .. })),
        "{:?}",
        result.stats.error
    );
    let _ = std::fs::remove_dir_all(&dir);
}
