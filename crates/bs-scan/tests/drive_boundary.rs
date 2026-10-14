//! The walker driven the way the application drives it: the drive, its mounts
//! and its used-bytes figure come from the platform crate.
#![cfg(target_os = "linux")]

mod support;

use bs_platform::drives::volume_for_path;
use bs_scan::{Boundary, ScanOptions};
use bs_tree::NodeFlags;
use support::*;

/// The mounts to enter: those of the drive that another mount does not already
/// show.
fn boundary_of(drive: &bs_platform::drives::Drive) -> Boundary {
    Boundary::with_mount_ids(
        drive
            .mounts
            .iter()
            .filter(|m| !m.covered_by_other_mount)
            .filter_map(|m| m.mount_id)
            .map(u64::from),
    )
}

#[test]
fn a_scan_started_from_the_platforms_drive_matches_du_and_stays_under_the_used_bytes() {
    let fx = Fixture::new("drive-scan");
    fx.file("a/one.bin", 30_000);
    fx.file("a/b/two.bin", 5_000);
    fx.settle();
    let drive = volume_for_path(&fx.root).expect("the drive holding the fixture");
    let used = drive.capacity.expect("capacity of a real drive").used;
    assert!(!drive.mounts.is_empty());

    let result = scan_with(
        &fx.root,
        ScanOptions {
            boundary: boundary_of(&drive),
            os_used_bytes: Some(used),
            ..ScanOptions::default()
        },
    );
    assert_clean(&result);
    assert_matches_du(&fx.root, &result);
    assert_eq!(result.snapshot.os_used_bytes(), Some(used));
    assert!(!result.snapshot.counted_exceeds_used());
}

#[test]
fn the_drives_mounts_decide_what_is_entered_below_dev() {
    let dev = std::path::Path::new("/dev");
    let drive = volume_for_path(dev).expect("the drive holding /dev");
    // /dev is a device filesystem; the mounts below it (devpts, shared
    // memory, ...) are other filesystems and not among its mounts.
    let result = scan_with(
        dev,
        ScanOptions {
            boundary: boundary_of(&drive),
            ..ScanOptions::default()
        },
    );
    result.snapshot.verify().unwrap();
    let Some(pts) = try_node_at(&result.snapshot, "pts") else {
        eprintln!("skipped: no /dev/pts");
        return;
    };
    assert!(pts.flags().contains(NodeFlags::NOT_FOLLOWED));
    assert_eq!(pts.children().count(), 0);
}
