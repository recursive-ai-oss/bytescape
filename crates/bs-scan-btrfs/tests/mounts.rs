//! Mounts below the scan's start directory: a subvolume mounted on a folder
//! of another (the usual `/` and `/home`), mounts of other file systems, and
//! the mounts that cannot be placed.
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

mod common;

use bs_scan::Boundary;
use bs_scan_btrfs::{BtrfsError, BtrfsOptions, MountPoint, Mounted, ProblemKind, Start};
use bs_tree::{NodeFlags, ReadErrorKind};
use common::memsrc::{MTIME, MemFs, MemSource};
use common::{Run, try_read, try_read_within};

const ROOT: u64 = 256;
const HOME_MOUNT: u64 = 50;
const OTHER_MOUNT: u64 = 51;
const START: Start = Start {
    tree: 256,
    inode: ROOT,
};

fn mount(path: &[&str], id: u64, mounted: Mounted) -> MountPoint {
    MountPoint {
        path: path.iter().map(|n| n.as_bytes().to_vec()).collect(),
        mount_id: id,
        device: 7 << 32 | id,
        mounted,
    }
}

fn here(tree: u64) -> Mounted {
    Mounted::Here(Start { tree, inode: ROOT })
}

fn read_with(src: &MemSource, ids: &[u64]) -> Result<Run, BtrfsError> {
    try_read_within(
        src,
        START,
        &BtrfsOptions::default(),
        Boundary::with_mount_ids(ids.iter().copied()),
    )
}

/// The common distribution layout: subvolumes `root` (256) and `home` (257)
/// side by side at the top level, mounted at `/` and `/home`. The scan starts
/// at `/`, in `root`; `/home` there is the empty folder `home` is mounted on.
fn distribution_layout() -> MemFs {
    let mut fs = MemFs::new();
    fs.make_tree(256);
    fs.make_tree(257);
    fs.subvolume(5, ROOT, "root", 256);
    fs.subvolume(5, ROOT, "home", 257);
    fs.plain(256, ROOT, "usr-file", 1 << 20);
    fs.mkdir(256, ROOT, "home");
    let user = fs.mkdir(257, ROOT, "alice");
    fs.plain(257, user, "photos.bin", 64 << 20);
    fs
}

#[test]
fn a_subvolume_mounted_below_the_root_is_read_at_the_folder_it_covers() {
    let mut src = distribution_layout().source();
    src.mounts = vec![mount(&["home"], HOME_MOUNT, here(257))];
    let run = read_with(&src, &[HOME_MOUNT]).unwrap();
    let m = run.flat();
    // The 64 MiB under the mount are the drive's, and the tree has them.
    assert_eq!(m["home/alice/photos.bin"].allocated, 64 << 20);
    let root = run.snapshot.root().unwrap();
    assert_eq!(root.allocated(), (1 << 20) + (64 << 20));
    assert!(!m["home"].flags.contains(NodeFlags::NOT_FOLLOWED));
    assert!(!m["home"].failed);
    assert!(run.snapshot.scan_complete());
    assert!(!root.bytes_are_lower_bound());
    assert_eq!(run.report.mounts_entered, 1);
    assert_eq!(run.report.mounts_not_followed, 0);
    assert_eq!(run.report.trees, 2);
    assert_eq!(run.report.diagnostics.total(), 0);
}

#[test]
fn the_mounted_directory_stands_in_with_its_own_numbers() {
    let mut src = distribution_layout().source();
    src.mounts = vec![mount(&["home"], HOME_MOUNT, here(257))];
    let run = read_with(&src, &[HOME_MOUNT]).unwrap();
    let m = run.flat();
    // The folder `home` in tree 256 was made one second after MTIME; the
    // mounted root (tree 257's inode 256) has MTIME itself.
    assert_eq!(m["home"].mtime, MTIME);
}

#[test]
fn a_mount_the_caller_leaves_out_is_not_followed_and_not_shown_as_empty() {
    let mut src = distribution_layout().source();
    src.mounts = vec![mount(&["home"], HOME_MOUNT, here(257))];
    // Default boundary: only the root's own mount.
    let run = try_read(&src, START, &BtrfsOptions::default()).unwrap();
    let m = run.flat();
    assert!(m["home"].flags.contains(NodeFlags::NOT_FOLLOWED));
    assert!(!m.keys().any(|k| k.starts_with("home/")));
    assert_eq!(run.report.mounts_entered, 0);
    assert_eq!(run.report.mounts_not_followed, 1);
    // The subvolume was not read into the tree.
    assert_eq!(run.report.trees, 1);
    assert_eq!(run.snapshot.root().unwrap().allocated(), 1 << 20);
}

#[test]
fn a_mount_of_another_file_system_hides_what_the_folder_holds() {
    let mut fs = MemFs::new();
    fs.make_tree(256);
    fs.subvolume(5, ROOT, "root", 256);
    fs.plain(256, ROOT, "mine.bin", 1 << 20);
    let boot = fs.mkdir(256, ROOT, "boot");
    // What the folder holds on this file system is covered by the mount.
    fs.plain(256, boot, "hidden.bin", 4 << 20);
    let mut src = fs.source();
    src.mounts = vec![mount(&["boot"], OTHER_MOUNT, Mounted::Other)];
    // Even if the caller listed it, another file system is never entered:
    // that is an error, not a silent skip, because the drive's bytes would
    // be missing.
    assert!(matches!(
        read_with(&src, &[OTHER_MOUNT]),
        Err(BtrfsError::MountNotPlaced(_))
    ));
    let run = read_with(&src, &[]).unwrap();
    let m = run.flat();
    assert!(m["boot"].flags.contains(NodeFlags::NOT_FOLLOWED));
    assert!(!m.contains_key("boot/hidden.bin"));
    assert_eq!(run.snapshot.root().unwrap().allocated(), 1 << 20);
    // The covered bytes are on this file system and are not hidden from the
    // report of what the tree does not hold.
    assert_eq!(run.report.unreachable.allocated, 4 << 20);
    assert_eq!(run.report.unreachable.inodes, 1);
}

#[test]
fn a_mount_of_the_drive_that_cannot_be_placed_ends_the_read() {
    let mut src = distribution_layout().source();
    // The source could not tell which subvolume is mounted.
    src.mounts = vec![mount(&["home"], HOME_MOUNT, Mounted::HereUnplaced)];
    let err = read_with(&src, &[HOME_MOUNT]).err().unwrap();
    assert!(matches!(err, BtrfsError::MountNotPlaced(_)), "{err}");
    // A mount that is not the drive's does not stop anything.
    assert!(read_with(&src, &[]).is_ok());

    // The folder it covers is not in the tree.
    let mut src = distribution_layout().source();
    src.mounts = vec![mount(&["no", "such"], HOME_MOUNT, here(257))];
    let err = read_with(&src, &[HOME_MOUNT]).err().unwrap();
    assert!(matches!(err, BtrfsError::MountNotPlaced(_)), "{err}");

    // The directory it says is mounted is not in the subvolume.
    let mut src = distribution_layout().source();
    src.mounts = vec![mount(
        &["home"],
        HOME_MOUNT,
        Mounted::Here(Start {
            tree: 257,
            inode: 9999,
        }),
    )];
    let err = read_with(&src, &[HOME_MOUNT]).err().unwrap();
    assert!(matches!(err, BtrfsError::MountNotPlaced(_)), "{err}");

    // The covered path leads through a file.
    let mut src = distribution_layout().source();
    src.mounts = vec![mount(&["usr-file", "x"], HOME_MOUNT, here(257))];
    let err = read_with(&src, &[HOME_MOUNT]).err().unwrap();
    assert!(matches!(err, BtrfsError::MountNotPlaced(_)), "{err}");
}

#[test]
fn a_mount_table_that_cannot_be_read_is_not_taken_for_an_empty_one() {
    let mut src = distribution_layout().source();
    src.mounts_fail = true;
    assert!(matches!(
        try_read(&src, START, &BtrfsOptions::default()),
        Err(BtrfsError::Io(_))
    ));
}

#[test]
fn too_many_mounts_are_refused() {
    let mut src = distribution_layout().source();
    src.mounts = (0..bs_scan_btrfs::MAX_MOUNTS as u64 + 1)
        .map(|i| mount(&["home"], 1000 + i, Mounted::Other))
        .collect();
    assert!(matches!(read_with(&src, &[]), Err(BtrfsError::TooLarge(_))));
}

#[test]
fn a_mount_of_no_concern_on_a_path_the_trees_lack_is_noted_and_ignored() {
    let mut src = distribution_layout().source();
    src.mounts = vec![
        mount(&["gone", "later"], OTHER_MOUNT, Mounted::Other),
        mount(&["home"], HOME_MOUNT, here(257)),
    ];
    let run = read_with(&src, &[HOME_MOUNT]).unwrap();
    assert_eq!(run.report.diagnostics.count(ProblemKind::MountNotPlaced), 1);
    assert!(run.flat().contains_key("home/alice/photos.bin"));
}

#[test]
fn mounts_inside_mounts_are_placed_through_them() {
    let mut fs = distribution_layout();
    fs.subvolume(5, ROOT, "extra", 258);
    // A folder of the home subvolume that a third subvolume is mounted on,
    // and a mount of another file system under that.
    let mut src = fs.source();
    // alice is inode 257 in tree 257 (the first inode made there).
    src.fs.mkdir(257, 257, "projects");
    src.fs.plain(258, ROOT, "big.bin", 8 << 20);
    src.fs.mkdir(258, ROOT, "nested");
    src.mounts = vec![
        mount(&["home", "alice", "projects"], 61, here(258)),
        mount(&["home"], HOME_MOUNT, here(257)),
        mount(&["home", "alice", "projects", "nested"], 62, Mounted::Other),
    ];
    let run = read_with(&src, &[HOME_MOUNT, 61]).unwrap();
    let m = run.flat();
    assert_eq!(m["home/alice/projects/big.bin"].allocated, 8 << 20);
    assert!(
        m["home/alice/projects/nested"]
            .flags
            .contains(NodeFlags::NOT_FOLLOWED)
    );
    assert_eq!(run.report.mounts_entered, 2);
    assert_eq!(run.report.mounts_not_followed, 1);
    assert_eq!(run.report.diagnostics.total(), 0);
    assert!(run.snapshot.scan_complete());
}

#[test]
fn a_mount_under_a_mount_that_is_not_followed_has_nothing_to_place() {
    let mut fs = distribution_layout();
    fs.mkdir(256, ROOT, "mnt");
    let mut src = fs.source();
    src.mounts = vec![
        mount(&["mnt"], OTHER_MOUNT, Mounted::Other),
        mount(&["mnt", "deeper"], 63, Mounted::Other),
    ];
    let run = read_with(&src, &[]).unwrap();
    assert!(run.flat()["mnt"].flags.contains(NodeFlags::NOT_FOLLOWED));
    assert_eq!(run.report.diagnostics.total(), 0);
    assert_eq!(run.report.mounts_not_followed, 1);
}

#[test]
fn a_mounted_subvolume_that_cannot_be_read_marks_the_folder_unreadable() {
    let mut src = distribution_layout().source();
    src.mounts = vec![mount(&["home"], HOME_MOUNT, here(257))];
    src.fail_trees.insert(257);
    let run = read_with(&src, &[HOME_MOUNT]).unwrap();
    let m = run.flat();
    assert!(m["home"].failed, "the covered folder must not look empty");
    assert!(run.snapshot.root().unwrap().bytes_are_lower_bound());
    assert!(!m.contains_key("home/alice"));
}

#[test]
fn a_mount_of_a_folder_the_scan_reaches_anyway_counts_its_files_once() {
    let mut fs = MemFs::new();
    fs.make_tree(256);
    fs.subvolume(5, ROOT, "root", 256);
    let dir = fs.mkdir(256, ROOT, "real");
    fs.plain(256, dir, "f.bin", 1 << 20);
    fs.mkdir(256, ROOT, "alias");
    let mut src = fs.source();
    // A bind mount of `real` on `alias`, listed by a careless caller.
    src.mounts = vec![mount(
        &["alias"],
        70,
        Mounted::Here(Start {
            tree: 256,
            inode: dir,
        }),
    )];
    let run = read_with(&src, &[70]).unwrap();
    let m = run.flat();
    assert_eq!(run.snapshot.root().unwrap().allocated(), 1 << 20);
    // Whichever of the two the scan comes to first holds the files; the
    // other is shown as not followed and the tree is not marked damaged.
    let held = [m.contains_key("real/f.bin"), m.contains_key("alias/f.bin")];
    assert_eq!(held.iter().filter(|h| **h).count(), 1, "{held:?}");
    assert!(!run.snapshot.root().unwrap().is_unreadable());
}

#[test]
fn the_failure_of_a_mounted_tree_is_a_read_error_kind() {
    let mut src = distribution_layout().source();
    src.mounts = vec![mount(&["home"], HOME_MOUNT, here(257))];
    src.fail_trees.insert(257);
    let run = read_with(&src, &[HOME_MOUNT]).unwrap();
    let id = run.flat()["home"].id;
    let node = run.snapshot.node(id).unwrap();
    assert_eq!(node.read_failure().unwrap().kind, ReadErrorKind::Io);
}

#[test]
fn two_mounts_on_one_folder_are_refused_when_one_belongs_to_the_drive() {
    // The source is to hand over only the mount on top; if it hands over
    // two for one folder, the reader cannot tell which a walk would see.
    let mut src = distribution_layout().source();
    src.mounts = vec![
        mount(&["home"], HOME_MOUNT, here(257)),
        mount(&["home"], OTHER_MOUNT, Mounted::Other),
    ];
    let err = read_with(&src, &[HOME_MOUNT]).err().unwrap();
    assert!(matches!(err, BtrfsError::MountNotPlaced(_)), "{err}");

    // Neither belongs to the drive: the folder is shown as not followed, once.
    let run = read_with(&src, &[]).unwrap();
    assert!(run.flat()["home"].flags.contains(NodeFlags::NOT_FOLLOWED));
    assert_eq!(run.report.mounts_not_followed, 1);
    assert_eq!(run.report.mounts_entered, 0);
    assert_eq!(run.report.diagnostics.total(), 0);
}

/// The top level holds the entries `home_name` (a subvolume entry for tree
/// 257) and a folder `folder_name` on which tree 257 is mounted, and the root
/// tree cannot be searched, so the entry cannot be placed.
fn unplaced_entry_of_a_mounted_subvolume(home_name: &str, folder_name: &str) -> Run {
    let mut fs = MemFs::new();
    fs.make_tree(257);
    fs.subvolume(5, ROOT, home_name, 257);
    fs.mkdir(5, ROOT, folder_name);
    fs.plain(257, ROOT, "photos.bin", 64 << 20);
    let mut src = fs.source();
    src.fail_trees.insert(bs_scan_btrfs::consts::ROOT_TREE_ID);
    src.mounts = vec![mount(&[folder_name], HOME_MOUNT, here(257))];
    let start = Start {
        tree: 5,
        inode: ROOT,
    };
    try_read_within(
        &src,
        start,
        &BtrfsOptions::default(),
        Boundary::with_mount_ids([HOME_MOUNT]),
    )
    .unwrap()
}

#[test]
fn an_unplaced_entry_is_not_entered_even_when_its_subvolume_was_loaded_by_a_mount() {
    // Whichever the walk meets first, the subvolume is read where the mount
    // puts it, and the entry that could not be placed is an unreadable folder:
    // it is not entered, so the real place is never dropped as a second way
    // in.
    for (entry, folder) in [("a-entry", "z-mount"), ("z-entry", "a-mount")] {
        let run = unplaced_entry_of_a_mounted_subvolume(entry, folder);
        let m = run.flat();
        assert_eq!(
            m[&format!("{folder}/photos.bin")].allocated,
            64 << 20,
            "{entry}/{folder}"
        );
        assert!(m[entry].failed, "{entry}/{folder}");
        assert!(
            !m.contains_key(&format!("{entry}/photos.bin")),
            "{entry}/{folder}"
        );
        assert_eq!(
            run.report.diagnostics.count(ProblemKind::SubvolumeSkipped),
            1,
            "{entry}/{folder}: only the failed lookup is noted, not a second way in"
        );
        let root = run.snapshot.root().unwrap();
        assert!(root.has_unreadable_below(), "{entry}/{folder}");
        assert!(root.bytes_are_lower_bound(), "{entry}/{folder}");
    }
}
