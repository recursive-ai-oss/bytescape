//! The reader's directory walk over mounts the real kernel makes.
//!
//! On a real mount point the listing names the covered directory's inode,
//! while opening the name lands on the root of what is mounted there, which
//! has another inode. The walk must tell that from a directory replaced under
//! it, and must neither drop the entry nor mark its parent unreadable.
//!
//! The mounts are made inside a private user and mount namespace
//! (`unshare --user --map-root-user --mount`), so no privilege is needed. The
//! inode table is built from the fixture's own file system, standing in for
//! the bulk read. Where the namespace cannot be made the test prints
//! `NOT RUN` and returns, unless `BYTESCAPE_REQUIRE_NAMESPACES` is set, which
//! makes that a failure.
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use bs_scan::{Boundary, ScanContext, ScanOptions};
use bs_scan_xfs::{BulkStat, FsNames, Inodes, XfsError, XfsOptions, read_filesystem};
use bs_tree::{Aggregator, NodeFlags, NodeRef, Snapshot};
use std::collections::BTreeMap;
use std::os::unix::fs::{DirEntryExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::Command;

const INNER: &str = "inside_the_namespace_real_mounts_are_walked_correctly";

struct Table(Vec<BulkStat>);

impl Inodes for Table {
    fn each(&self, each: &mut dyn FnMut(BulkStat) -> bool) -> Result<(), XfsError> {
        for b in &self.0 {
            if !each(*b) {
                break;
            }
        }
        Ok(())
    }
}

fn row(ino: u64, m: &std::fs::Metadata) -> BulkStat {
    BulkStat {
        ino,
        size: m.size(),
        blocks: m.blocks(),
        block_size: 512,
        mtime: m.mtime(),
        nlink: m.nlink() as u32,
        mode: m.mode() as u16,
    }
}

/// Every inode of the device that holds `top`, as the bulk read would give
/// them, including the covered directory under each mount point.
fn inode_table(top: &Path) -> Table {
    let dev = std::fs::metadata(top).unwrap().dev();
    let mut rows = Vec::new();
    let mut stack = vec![top.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let m = std::fs::symlink_metadata(&dir).unwrap();
        if m.dev() != dev {
            continue;
        }
        rows.push(row(m.ino(), &m));
        if !m.is_dir() {
            continue;
        }
        for e in std::fs::read_dir(&dir).unwrap() {
            let e = e.unwrap();
            let md = std::fs::symlink_metadata(e.path()).unwrap();
            if e.ino() != md.ino() {
                // A mount point: the listing's inode is the covered
                // directory's, which the file system holds as an empty
                // directory.
                rows.push(BulkStat {
                    ino: e.ino(),
                    size: 6,
                    blocks: 0,
                    block_size: 512,
                    mtime: 0,
                    nlink: 2,
                    mode: 0o040_755,
                });
            }
            stack.push(e.path());
        }
    }
    rows.sort_by_key(|b| b.ino);
    rows.dedup_by_key(|b| b.ino);
    Table(rows)
}

/// The mount ID of the mount at `path`, from the mount table.
fn mount_id(path: &Path) -> u64 {
    let text = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
    text.lines()
        .find_map(|l| {
            let mut f = l.split_whitespace();
            let id: u64 = f.next()?.parse().ok()?;
            (f.nth(3)? == path.to_str()?).then_some(id)
        })
        .unwrap_or_else(|| panic!("{} is not a mount point", path.display()))
}

struct Out {
    snapshot: Snapshot,
    problems: u64,
    unreachable: u64,
}

fn read(root: &Path, table: &Table, boundary: Boundary) -> Out {
    let (ctx, rx) = ScanContext::channel(
        ScanOptions {
            boundary,
            ..ScanOptions::default()
        },
        4096,
    );
    let options = XfsOptions {
        threads: Some(3),
        ..XfsOptions::default()
    };
    let (_, report) =
        read_filesystem(table, &FsNames::new(root), b"/fixture", &options, &ctx).unwrap();
    drop(ctx);
    let mut agg = Aggregator::new();
    for b in rx.try_iter() {
        assert_eq!(agg.apply(&b).rejected, 0);
    }
    agg.finish();
    let snapshot = agg.snapshot();
    snapshot.verify().unwrap();
    Out {
        snapshot,
        problems: report.diagnostics.total(),
        unreachable: report.unreachable.inodes,
    }
}

fn paths(snap: &Snapshot) -> BTreeMap<String, NodeFlags> {
    let mut out = BTreeMap::new();
    let mut stack: Vec<(NodeRef<'_>, String)> = vec![(snap.root().unwrap(), String::new())];
    while let Some((n, p)) = stack.pop() {
        for c in n.children() {
            let path = format!("{p}{}{}", if p.is_empty() { "" } else { "/" }, {
                String::from_utf8_lossy(c.name())
            });
            out.insert(path.clone(), c.flags());
            stack.push((c, path));
        }
    }
    out
}

fn sh(script: &str) {
    let status = Command::new("sh").arg("-c").arg(script).status().unwrap();
    assert!(status.success(), "failed: {script}");
}

/// Runs inside the namespace; outside it (a plain `cargo test`) it has
/// nothing to do.
#[test]
fn inside_the_namespace_real_mounts_are_walked_correctly() {
    let Some(dir) = std::env::var_os("BS_XFS_MOUNT_DIR") else {
        return;
    };
    let top = PathBuf::from(dir);
    let scan = top.join("scan");
    std::fs::create_dir_all(scan.join("other-fs")).unwrap();
    std::fs::create_dir_all(scan.join("bound")).unwrap();
    std::fs::create_dir_all(top.join("elsewhere/dir")).unwrap();
    std::fs::write(scan.join("mine.bin"), vec![b'm'; 8192]).unwrap();
    std::fs::write(top.join("elsewhere/dir/bound.bin"), vec![b'b'; 16384]).unwrap();
    // Another file system, and a second mount of this one.
    sh(&format!(
        "mount -t tmpfs -o size=8m none '{}'",
        scan.join("other-fs").display()
    ));
    std::fs::write(scan.join("other-fs/foreign.bin"), vec![b'f'; 1 << 20]).unwrap();
    sh(&format!(
        "mount --bind '{}' '{}'",
        top.join("elsewhere/dir").display(),
        scan.join("bound").display()
    ));
    // The kernel hands a different inode to the open than to the listing.
    for name in ["other-fs", "bound"] {
        let listed = std::fs::read_dir(&scan)
            .unwrap()
            .map(Result::unwrap)
            .find(|e| e.file_name() == name)
            .unwrap()
            .ino();
        let opened = std::fs::metadata(scan.join(name)).unwrap().ino();
        assert_ne!(listed, opened, "{name} is a mount point");
    }
    let root = scan.canonicalize().unwrap();
    let table = inode_table(&top.canonicalize().unwrap());
    let own = 8192;

    // Left to itself the scan enters neither mount, and nothing is lost: the
    // mount points are shown as not followed, their parent is whole.
    let out = read(&root, &table, Boundary::root_mount());
    let p = paths(&out.snapshot);
    assert!(p["other-fs"].contains(NodeFlags::NOT_FOLLOWED), "{p:?}");
    assert!(p["bound"].contains(NodeFlags::NOT_FOLLOWED), "{p:?}");
    assert!(!p.keys().any(|k| k.starts_with("other-fs/")), "{p:?}");
    assert!(!p.keys().any(|k| k.starts_with("bound/")), "{p:?}");
    let r = out.snapshot.root().unwrap();
    assert!(r.is_complete(), "the root is complete");
    assert!(!r.is_unreadable(), "the root is not marked unreadable");
    assert!(!r.bytes_are_lower_bound());
    assert!(p.contains_key("mine.bin"));
    assert!(r.allocated() >= own && r.allocated() < own + (1 << 16));
    assert_eq!(out.problems, 0, "no entry is called corrupt");

    // The caller counts both mounts as part of the drive. The tmpfs is a
    // different file system (another device), which this inode table cannot
    // describe, so it stays out whatever the caller says; the second mount of
    // this file system is entered under the inode it really has.
    let ids = [
        mount_id(&root.join("other-fs")),
        mount_id(&root.join("bound")),
    ];
    let out = read(&root, &table, Boundary::with_mount_ids(ids));
    let p = paths(&out.snapshot);
    assert!(p["other-fs"].contains(NodeFlags::NOT_FOLLOWED), "{p:?}");
    assert!(!p.keys().any(|k| k.starts_with("other-fs/")), "{p:?}");
    assert!(!p["bound"].contains(NodeFlags::NOT_FOLLOWED), "{p:?}");
    assert!(p.contains_key("bound/bound.bin"), "{p:?}");
    let r = out.snapshot.root().unwrap();
    assert!(r.is_complete() && !r.is_unreadable() && !r.bytes_are_lower_bound());
    assert!(r.allocated() >= own + 16384 && r.allocated() < own + 16384 + (1 << 16));
    assert_eq!(out.problems, 0, "no entry is called corrupt");
    // The bound directory's inode is accounted for by the entry that led
    // there; what is left over is only what lies outside the scan root.
    assert!(out.unreachable <= 6, "{}", out.unreachable);
}

#[test]
fn real_mount_points_are_not_dropped_as_corrupt_in_a_private_namespace() {
    if std::env::var_os("BS_XFS_MOUNT_DIR").is_some() {
        // This is the inner run; the other test does the work.
        return;
    }
    let probe = Command::new("unshare")
        .args(["--user", "--map-root-user", "--mount", "true"])
        .output();
    if !matches!(&probe, Ok(o) if o.status.success()) {
        assert!(
            std::env::var_os("BYTESCAPE_REQUIRE_NAMESPACES").is_none(),
            "BYTESCAPE_REQUIRE_NAMESPACES is set but a user and mount namespace cannot be made"
        );
        eprintln!("NOT RUN: cannot create a user and mount namespace here");
        return;
    }
    let top =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("xfs-mounts-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&top);
    std::fs::create_dir_all(&top).unwrap();
    let exe = std::env::current_exe().unwrap();
    let script = format!(
        "exec '{}' {INNER} --exact --test-threads=1 --nocapture",
        exe.display()
    );
    let out = Command::new("unshare")
        .args(["--user", "--map-root-user", "--mount", "sh", "-c", &script])
        .env("BS_XFS_MOUNT_DIR", &top)
        .env("RUST_BACKTRACE", "0")
        .output()
        .unwrap();
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&top);
    assert!(out.status.success(), "the namespace run failed:\n{text}");
    assert!(
        text.contains("1 passed"),
        "the inner test did not run:\n{text}"
    );
}
