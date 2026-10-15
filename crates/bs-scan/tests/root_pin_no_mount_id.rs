//! A pinned scan on a kernel that gives no mount IDs: the pin cannot be
//! checked, so the scan is refused, never run unbound. One test, because it
//! switches the fallback on for the whole process.
#![cfg(target_os = "linux")]

mod support;

use bs_scan::{RootPin, ScanError, ScanOptions, rawdir};
use std::sync::Arc;
use support::*;

#[test]
fn a_pin_that_cannot_be_checked_refuses_the_scan() {
    let fx = Fixture::new("pin-no-ids");
    fx.file("a/b", 1234);
    fx.settle();
    // The caller judged the folder on a kernel that does say; the scan runs
    // where it does not.
    let pin = RootPin::from_fd(Arc::new(rawdir::open_root(&fx.root).unwrap())).unwrap();
    let pinned_key = pin.key();
    bs_scan::force_device_fallback(true);

    // Without a pin the device fallback works as before.
    let plain = scan(&fx.root);
    assert_clean(&plain);

    let result = scan_with(&fx.root, ScanOptions::default().pin_root(pin));
    // A pin cannot even be made where the system gives no ID.
    let made = RootPin::from_fd(Arc::new(rawdir::open_root(&fx.root).unwrap()));
    bs_scan::force_device_fallback(false);
    assert_eq!(made.unwrap_err().kind(), std::io::ErrorKind::Unsupported);
    match result.stats.error {
        Some(ScanError::RootMoved {
            pinned,
            found: None,
        }) => assert_eq!(pinned, pinned_key),
        other => panic!("expected RootMoved with nothing found, got {other:?}"),
    }
    assert!(result.snapshot.root().is_none());
    assert_eq!(result.stats.progress.batches, 0);
}
