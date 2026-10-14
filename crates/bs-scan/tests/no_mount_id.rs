//! The walker on a kernel that gives no mount IDs: boundaries fall back to
//! device numbers. This file holds one test because it switches the fallback
//! on for the whole process.
#![cfg(target_os = "linux")]

mod support;

use bs_scan::{Boundary, ScanOptions};
use bs_tree::NodeFlags;
use std::os::unix::fs::MetadataExt;
use support::*;

/// The walker's device number: major in the high 32 bits, minor in the low.
fn walker_device(st_dev: u64) -> u64 {
    let major = ((st_dev >> 8) & 0xfff) | ((st_dev >> 32) & !0xfff);
    let minor = (st_dev & 0xff) | ((st_dev >> 12) & !0xff);
    (major << 32) | minor
}

#[test]
fn without_mount_ids_the_device_decides() {
    bs_scan::force_device_fallback(true);

    // An ordinary tree is unaffected.
    let fx = Fixture::new("fallback-plain");
    fx.file("a/b", 1234);
    fx.settle();
    let result = scan(&fx.root);
    assert_clean(&result);
    assert_matches_du(&fx.root, &result);

    // Another filesystem below /dev is refused by its device number ...
    let pts = std::path::Path::new("/dev/pts");
    let Ok(meta) = std::fs::metadata(pts) else {
        eprintln!("skipped the /dev part: no /dev/pts");
        return;
    };
    if meta.dev() == std::fs::metadata("/dev").unwrap().dev() {
        eprintln!("skipped the /dev part: /dev/pts is not a separate filesystem");
        return;
    }
    let refused = scan(std::path::Path::new("/dev"));
    let node = node_at(&refused.snapshot, "pts");
    assert!(node.flags().contains(NodeFlags::NOT_FOLLOWED));
    assert_eq!(node.children().count(), 0);

    // ... and entered when the caller lists the device.
    let entered = scan_with(
        std::path::Path::new("/dev"),
        ScanOptions {
            boundary: Boundary::root_mount().and_devices([walker_device(meta.dev())]),
            ..ScanOptions::default()
        },
    );
    let node = node_at(&entered.snapshot, "pts");
    assert!(!node.flags().contains(NodeFlags::NOT_FOLLOWED));
    assert!(node.is_complete());
    bs_scan::force_device_fallback(false);
}
