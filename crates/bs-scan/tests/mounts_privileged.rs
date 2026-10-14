//! Mount-boundary tests that need real mounts, so root. They run only when the
//! machine is declared disposable:
//!
//! ```text
//! BYTESCAPE_DISPOSABLE_HOST=1 cargo test -p bs-scan --test mounts_privileged
//! ```
//!
//! With the variable unset every test returns at once and prints that it did
//! not run. With it set, a test that cannot do its job (no root, no `mount`,
//! no `mkfs.btrfs`) fails instead of passing quietly.
#![cfg(target_os = "linux")]

mod support;

use bs_scan::{Boundary, ScanOptions};
use bs_tree::{NodeFlags, NodeKind};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;
use support::*;

fn enabled(test: &str) -> bool {
    if std::env::var("BYTESCAPE_DISPOSABLE_HOST").as_deref() == Ok("1") {
        true
    } else {
        eprintln!("NOT RUN: {test} needs BYTESCAPE_DISPOSABLE_HOST=1 and root");
        false
    }
}

fn run(cmd: &str, args: &[&str]) {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("cannot run {cmd}: {e}"));
    assert!(
        out.status.success(),
        "{cmd} {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The kernel's mount ID of the mount at `point`.
fn mount_id_of(point: &Path) -> u64 {
    let text = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
    text.lines()
        .filter_map(|l| {
            let mut f = l.split(' ');
            let id: u64 = f.next()?.parse().ok()?;
            let at = f.nth(3)?;
            (Path::new(at) == point).then_some(id)
        })
        .next_back()
        .unwrap_or_else(|| panic!("{} is not a mount point", point.display()))
}

/// Unmounts when dropped, so a failing test does not leave mounts behind.
struct Mounted(std::path::PathBuf);

impl Drop for Mounted {
    fn drop(&mut self) {
        let _ = Command::new("umount").arg("-l").arg(&self.0).status();
    }
}

#[test]
fn a_tmpfs_mounted_inside_the_tree_is_entered_only_when_listed() {
    if !enabled("a_tmpfs_mounted_inside_the_tree_is_entered_only_when_listed") {
        return;
    }
    let fx = Fixture::new("tmpfs-inside");
    fx.file("outer/a", 1000);
    fx.dir("mnt");
    run(
        "mount",
        &["-t", "tmpfs", "tmpfs", fx.path("mnt").to_str().unwrap()],
    );
    let _guard = Mounted(fx.path("mnt"));
    std::fs::write(fx.path("mnt/inside-the-mount"), vec![0u8; 50_000]).unwrap();

    // Default: only the root's own mount.
    let result = scan(&fx.root);
    assert_clean(&result);
    let mnt = node_at(&result.snapshot, "mnt");
    assert!(mnt.flags().contains(NodeFlags::NOT_FOLLOWED));
    assert_eq!(mnt.children().count(), 0);
    assert_eq!(mnt.allocated(), 0);
    assert!(try_node_at(&result.snapshot, "mnt/inside-the-mount").is_none());

    // Listed: entered and counted.
    let id = mount_id_of(&fx.path("mnt"));
    let result = scan_with(
        &fx.root,
        ScanOptions {
            boundary: Boundary::with_mount_ids([id]),
            ..ScanOptions::default()
        },
    );
    assert_clean(&result);
    assert_eq!(
        node_at(&result.snapshot, "mnt/inside-the-mount").logical(),
        50_000
    );
}

#[test]
fn a_bind_mount_of_a_folder_already_reached_is_not_counted_twice() {
    if !enabled("a_bind_mount_of_a_folder_already_reached_is_not_counted_twice") {
        return;
    }
    let fx = Fixture::new("bind");
    fx.file("real/data.bin", 70_000);
    fx.dir("alias");
    run(
        "mount",
        &[
            "--bind",
            fx.path("real").to_str().unwrap(),
            fx.path("alias").to_str().unwrap(),
        ],
    );
    let _guard = Mounted(fx.path("alias"));

    let once = scan(&fx.root);
    assert_clean(&once);
    assert!(
        node_at(&once.snapshot, "alias")
            .flags()
            .contains(NodeFlags::NOT_FOLLOWED)
    );
    let counted_once = once.snapshot.root().unwrap().logical();
    assert_eq!(counted_once, 70_000);

    // A caller that lists the bind mount asks for the folder twice.
    let id = mount_id_of(&fx.path("alias"));
    let twice = scan_with(
        &fx.root,
        ScanOptions {
            boundary: Boundary::with_mount_ids([id]),
            ..ScanOptions::default()
        },
    );
    assert_eq!(twice.snapshot.root().unwrap().logical(), 2 * counted_once);
}

/// A nested btrfs subvolume has the mount ID of its parent and a device number
/// of its own; the scan must go into it.
#[test]
fn a_nested_btrfs_subvolume_is_entered_under_the_default_boundary() {
    if !enabled("a_nested_btrfs_subvolume_is_entered_under_the_default_boundary") {
        return;
    }
    let fx = Fixture::new("btrfs");
    let image = fx.path("fs.img");
    let mnt = fx.path("mnt");
    std::fs::create_dir(&mnt).unwrap();
    std::fs::File::create(&image)
        .unwrap()
        .set_len(256 << 20)
        .unwrap();
    run("mkfs.btrfs", &["-q", image.to_str().unwrap()]);
    run(
        "mount",
        &["-o", "loop", image.to_str().unwrap(), mnt.to_str().unwrap()],
    );
    let _guard = Mounted(mnt.clone());
    run(
        "btrfs",
        &["subvolume", "create", mnt.join("sub").to_str().unwrap()],
    );
    std::fs::write(mnt.join("top.bin"), vec![1u8; 30_000]).unwrap();
    std::fs::write(mnt.join("sub/inside.bin"), vec![2u8; 40_000]).unwrap();
    run("sync", &[]);

    let dev_top = std::fs::metadata(&mnt).unwrap().dev();
    let dev_sub = std::fs::metadata(mnt.join("sub")).unwrap().dev();
    assert_ne!(dev_top, dev_sub, "a subvolume has its own device number");

    let result = scan(&mnt);
    assert_clean(&result);
    let inside = node_at(&result.snapshot, "sub/inside.bin");
    assert_eq!(inside.kind(), NodeKind::File);
    assert_eq!(inside.logical(), 40_000);
    assert!(
        !node_at(&result.snapshot, "sub")
            .flags()
            .contains(NodeFlags::NOT_FOLLOWED)
    );
}
