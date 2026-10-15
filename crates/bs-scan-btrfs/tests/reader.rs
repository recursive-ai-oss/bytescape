//! The reader over file systems held in memory as tree items: sizes, links,
//! sharing, subvolumes, and everything that can go wrong in them.

#![cfg(target_os = "linux")]

mod common;

use bs_scan::{ScanContext, ScanEnd, ScanOptions};
use bs_scan_btrfs::{BtrfsError, BtrfsOptions, ProblemKind, Start, read_filesystem};
use bs_tree::{NodeFlags, NodeKind};
use common::memsrc::{Ext, MemFs};
use common::{START, read, try_read};

const FS: u64 = 5;
const ROOT: u64 = 256;

#[test]
fn files_directories_links_and_special_files_are_described() {
    let mut fs = MemFs::new();
    let d = fs.mkdir(FS, ROOT, "docs");
    fs.plain(FS, d, "a.txt", 10_000);
    fs.plain(FS, ROOT, "b.bin", 4096);
    fs.symlink(FS, ROOT, "link", "docs/a.txt");
    fs.fifo(FS, ROOT, "pipe");
    fs.file(FS, ROOT, b"empty", 0, &[]);
    let run = read(&fs.source());
    let m = run.flat();
    assert_eq!(m["docs"].kind, NodeKind::Dir);
    assert_eq!((m["docs"].logical, m["docs"].allocated), (0, 0));
    assert_eq!(m["docs"].mtime, common::memsrc::MTIME + 1);
    assert_eq!(
        (m["docs/a.txt"].logical, m["docs/a.txt"].allocated),
        (10_000, 10_000)
    );
    assert_eq!(m["docs/a.txt"].mtime, common::memsrc::MTIME + 2);
    assert_eq!((m["b.bin"].logical, m["b.bin"].allocated), (4096, 4096));
    assert_eq!(m["link"].kind, NodeKind::Symlink);
    assert!(m["link"].flags.contains(NodeFlags::NOT_FOLLOWED));
    assert_eq!(
        (m["link"].logical, m["link"].allocated),
        (10, 10),
        "the target is kept in the tree"
    );
    assert_eq!(m["pipe"].kind, NodeKind::Other);
    assert_eq!((m["empty"].logical, m["empty"].allocated), (0, 0));
    let root = run.snapshot.root().unwrap();
    assert_eq!(root.name(), b"/mnt");
    assert_eq!(root.allocated(), 14_096 + 10);
    assert_eq!(root.file_count(), 5);
    assert!(run.snapshot.scan_complete());
    assert!(!root.bytes_are_lower_bound());
    assert_eq!(run.end, ScanEnd::Completed);
    assert_eq!(
        run.report.diagnostics.total(),
        0,
        "{:?}",
        run.report.diagnostics
    );
    assert_eq!(run.report.inodes, 7);
    assert_eq!(run.report.trees, 1);
    assert!(run.report.sharing_exact);
    assert_eq!(run.report.unreachable.inodes, 0);
}

#[test]
fn names_are_kept_as_the_bytes_they_are() {
    let mut fs = MemFs::new();
    fs.file(FS, ROOT, b"new\nline", 1, &[Ext::Inline]);
    fs.file(FS, ROOT, b"\xff\xfe-not-utf8", 1, &[Ext::Inline]);
    let long = vec![b'n'; 255];
    fs.file(FS, ROOT, &long, 1, &[Ext::Inline]);
    let run = read(&fs.source());
    let root = run.snapshot.root().unwrap();
    let names: std::collections::BTreeSet<Vec<u8>> =
        root.children().map(|c| c.name().to_vec()).collect();
    assert!(names.contains(&b"new\nline"[..]));
    assert!(names.contains(&b"\xff\xfe-not-utf8"[..]));
    assert!(names.contains(&long[..]));
}

#[test]
fn an_extent_two_files_use_is_counted_once_and_both_are_shared() {
    let mut fs = MemFs::new();
    let addr = fs.fresh_extent(1 << 20);
    let a = fs.file(
        FS,
        ROOT,
        b"original",
        1 << 20,
        &[Ext::Disk(addr, 1 << 20, false)],
    );
    let _ = a;
    fs.file(
        FS,
        ROOT,
        b"reflink",
        1 << 20,
        &[Ext::Disk(addr, 1 << 20, false)],
    );
    fs.plain(FS, ROOT, "private", 8192);
    let run = read(&fs.source());
    let m = run.flat();
    let first_second = (m["original"].allocated, m["reflink"].allocated);
    // Whichever is emitted first carries the bytes, the other none.
    assert!(
        first_second == (1 << 20, 0) || first_second == (0, 1 << 20),
        "{first_second:?}"
    );
    for n in ["original", "reflink"] {
        assert!(m[n].flags.contains(NodeFlags::SHARED), "{n}");
        assert_eq!(m[n].exclusive, Some(0), "{n}");
        assert_eq!(m[n].logical, 1 << 20, "{n}");
    }
    assert!(!m["private"].flags.contains(NodeFlags::SHARED));
    assert_eq!(m["private"].exclusive, None);
    let root = run.snapshot.root().unwrap();
    assert_eq!(
        root.allocated(),
        (1 << 20) + 8192,
        "the extent is in the total once"
    );
    assert_eq!(run.report.sharing.files, 2);
    assert_eq!(run.report.sharing.exclusive, 0);

    // What deleting either frees: at most what it is counted with, at least
    // nothing, since the other still uses the bytes.
    for n in ["original", "reflink"] {
        let est = run.snapshot.delete_would_free(m[n].id).unwrap();
        assert_eq!(est.at_least, 0, "{n}");
    }
    // Deleting both together: the figure is a lower bound (it cannot tell
    // that the two share only with each other), the upper bound is exact.
    let est = run
        .snapshot
        .delete_would_free(bs_tree::NodeId::ROOT)
        .unwrap();
    assert_eq!(est.at_most, (1 << 20) + 8192);
    assert_eq!(est.at_least, 8192);
}

#[test]
fn a_file_with_a_private_part_and_a_shared_part_carries_both_figures() {
    let mut fs = MemFs::new();
    let shared = fs.fresh_extent(65536);
    let mine = fs.fresh_extent(131_072);
    fs.file(FS, ROOT, b"base", 65536, &[Ext::Disk(shared, 65536, false)]);
    fs.file(
        FS,
        ROOT,
        b"derived",
        65536 + 131_072,
        &[
            Ext::Disk(shared, 65536, false),
            Ext::Disk(mine, 131_072, false),
        ],
    );
    let run = read(&fs.source());
    let m = run.flat();
    assert_eq!(m["base"].allocated, 65536);
    assert_eq!(m["base"].exclusive, Some(0));
    assert_eq!(
        m["derived"].allocated, 131_072,
        "the shared extent was counted at base"
    );
    assert_eq!(m["derived"].exclusive, Some(131_072));
    let est = run.snapshot.delete_would_free(m["derived"].id).unwrap();
    assert_eq!((est.at_least, est.at_most), (131_072, 131_072));
}

#[test]
fn an_extent_a_file_names_twice_is_counted_once_and_is_not_shared() {
    let mut fs = MemFs::new();
    let addr = fs.fresh_extent(8192);
    fs.file(
        FS,
        ROOT,
        b"twice",
        16_384,
        &[Ext::Disk(addr, 8192, false), Ext::Disk(addr, 8192, false)],
    );
    let run = read(&fs.source());
    let m = run.flat();
    assert_eq!(m["twice"].allocated, 8192);
    assert!(!m["twice"].flags.contains(NodeFlags::SHARED));
    assert_eq!(m["twice"].logical, 16_384);
}

#[test]
fn holes_cost_nothing_inline_data_its_stored_bytes_and_preallocation_its_reservation() {
    let mut fs = MemFs::new();
    let addr = fs.fresh_extent(1 << 20);
    fs.file(FS, ROOT, b"inline", 100, &[Ext::Inline]);
    fs.file(FS, ROOT, b"holey", 1 << 20, &[Ext::Hole]);
    fs.file(
        FS,
        ROOT,
        b"reserved",
        1 << 20,
        &[Ext::Prealloc(addr, 1 << 20)],
    );
    let run = read(&fs.source());
    let m = run.flat();
    assert_eq!(m["inline"].allocated, 100);
    assert_eq!(m["holey"].allocated, 0);
    assert_eq!(m["reserved"].allocated, 1 << 20);
    // A file much bigger than what it holds on disk is sparse.
    assert!(m["holey"].flags.contains(NodeFlags::SPARSE_OR_COMPRESSED));
    assert!(!m["inline"].flags.contains(NodeFlags::SPARSE_OR_COMPRESSED));
    assert!(
        !m["reserved"]
            .flags
            .contains(NodeFlags::SPARSE_OR_COMPRESSED)
    );
}

#[test]
fn compression_reports_the_logical_size_and_the_size_on_disk() {
    let mut fs = MemFs::new();
    let addr = fs.fresh_extent(4096);
    fs.file(
        FS,
        ROOT,
        b"packed.log",
        1 << 20,
        &[Ext::Disk(addr, 4096, true)],
    );
    let run = read(&fs.source());
    let m = run.flat();
    assert_eq!(
        (m["packed.log"].logical, m["packed.log"].allocated),
        (1 << 20, 4096)
    );
    assert!(
        m["packed.log"]
            .flags
            .contains(NodeFlags::SPARSE_OR_COMPRESSED)
    );
    assert_eq!(run.report.compression.files, 1);
    assert_eq!(run.report.compression.logical, 1 << 20);
    assert_eq!(run.report.compression.on_disk, 4096);
}

#[test]
fn hard_links_count_once_and_are_flagged() {
    let mut fs = MemFs::new();
    let d = fs.mkdir(FS, ROOT, "d");
    let f = fs.plain(FS, ROOT, "one", 20_000);
    fs.link(FS, d, "two", f);
    fs.link(FS, d, "three", f);
    fs.plain(FS, ROOT, "lonely", 20_000);
    let run = read(&fs.source());
    let m = run.flat();
    let names = ["one", "d/two", "d/three"];
    for n in names {
        assert!(m[n].flags.contains(NodeFlags::HARD_LINK), "{n}");
    }
    let counted: u64 = names.iter().map(|n| m[*n].allocated).sum();
    assert_eq!(counted, 20_000);
    let dupes = names
        .iter()
        .filter(|n| m[**n].flags.contains(NodeFlags::HARD_LINK_DUPLICATE))
        .count();
    assert_eq!(dupes, 2);
    assert!(!m["lonely"].flags.contains(NodeFlags::HARD_LINK));
    assert_eq!(run.snapshot.root().unwrap().allocated(), 40_000);
    assert_eq!(run.snapshot.root().unwrap().file_count(), 4);
}

#[test]
fn nested_subvolumes_are_entered_and_look_like_directories() {
    let mut fs = MemFs::new();
    fs.plain(FS, ROOT, "outer", 4096);
    fs.subvolume(FS, ROOT, "snapshots", 300);
    let inner = fs.mkdir(300, ROOT, "deeper");
    fs.plain(300, inner, "inner.bin", 8192);
    fs.subvolume(300, ROOT, "grandchild", 301);
    fs.plain(301, ROOT, "g.bin", 16_384);
    let run = read(&fs.source());
    let m = run.flat();
    assert_eq!(m["snapshots"].kind, NodeKind::Dir);
    assert_eq!(m["snapshots/deeper/inner.bin"].allocated, 8192);
    assert_eq!(m["snapshots/grandchild"].kind, NodeKind::Dir);
    assert_eq!(m["snapshots/grandchild/g.bin"].allocated, 16_384);
    // The subvolume's own root directory gives the directory's time.
    assert_eq!(m["snapshots"].mtime, common::memsrc::MTIME);
    assert_eq!(
        run.snapshot.root().unwrap().allocated(),
        4096 + 8192 + 16_384
    );
    assert_eq!(run.report.trees, 3);
    assert!(run.snapshot.scan_complete());
}

#[test]
fn a_snapshot_shares_everything_with_its_origin() {
    let mut fs = MemFs::new();
    let a = fs.fresh_extent(65536);
    let b = fs.fresh_extent(32768);
    fs.file(FS, ROOT, b"a", 65536, &[Ext::Disk(a, 65536, false)]);
    fs.file(FS, ROOT, b"b", 32768, &[Ext::Disk(b, 32768, false)]);
    fs.subvolume(FS, ROOT, "snap", 300);
    // The snapshot holds the same two files, as a snapshot does.
    fs.file(300, ROOT, b"a", 65536, &[Ext::Disk(a, 65536, false)]);
    fs.file(300, ROOT, b"b", 32768, &[Ext::Disk(b, 32768, false)]);
    let run = read(&fs.source());
    let m = run.flat();
    for p in ["a", "b", "snap/a", "snap/b"] {
        assert!(m[p].flags.contains(NodeFlags::SHARED), "{p}");
        assert_eq!(m[p].exclusive, Some(0), "{p}");
    }
    // Counted once, wherever the walk meets them first.
    assert_eq!(run.snapshot.root().unwrap().allocated(), 65536 + 32768);
    let snap = run.snapshot.delete_would_free(m["snap"].id).unwrap();
    assert_eq!(
        snap.at_least, 0,
        "deleting the snapshot frees nothing the origin uses"
    );
}

#[test]
fn a_snapshot_outside_the_scan_still_makes_the_extents_shared() {
    let build = || {
        let mut fs = MemFs::new();
        let a = fs.fresh_extent(65536);
        fs.file(FS, ROOT, b"a", 65536, &[Ext::Disk(a, 65536, false)]);
        fs.plain(FS, ROOT, "b", 4096);
        // A subvolume that no directory of the scan names (a sibling
        // snapshot kept elsewhere) holds `a` too.
        fs.make_tree(400);
        fs.file(
            400,
            ROOT,
            b"copy-of-a",
            65536,
            &[Ext::Disk(a, 65536, false)],
        );
        fs
    };
    let run = read(&build().source());
    let m = run.flat();
    assert!(m["a"].flags.contains(NodeFlags::SHARED));
    assert_eq!(m["a"].exclusive, Some(0));
    assert!(!m["b"].flags.contains(NodeFlags::SHARED));
    assert_eq!(run.report.census_trees, 1);
    assert!(run.report.sharing_exact);
    assert_eq!(run.report.trees, 1, "the scan did not enter it");
    // Switched off: nothing was checked against the rest of the file system,
    // so no file is claimed to be private, and the report says so.
    let options = BtrfsOptions {
        census: false,
        ..BtrfsOptions::default()
    };
    let run = try_read(&build().source(), START, &options).unwrap();
    let m = run.flat();
    for n in ["a", "b"] {
        assert!(m[n].flags.contains(NodeFlags::SHARED), "{n}");
        assert_eq!(m[n].exclusive, Some(0), "{n}");
    }
    assert!(!run.report.sharing_exact);
    assert_eq!(run.report.census_trees, 0);
    let est = run.snapshot.delete_would_free(m["a"].id).unwrap();
    assert_eq!((est.at_least, est.at_most), (0, 65536));
}

#[test]
fn a_subvolume_the_census_cannot_read_makes_everything_shared() {
    let mut fs = MemFs::new();
    fs.plain(FS, ROOT, "a", 4096);
    fs.plain(FS, ROOT, "b", 4096);
    fs.make_tree(400);
    let mut src = fs.source();
    src.fail_trees.insert(400);
    let run = read(&src);
    let m = run.flat();
    for n in ["a", "b"] {
        assert!(m[n].flags.contains(NodeFlags::SHARED), "{n}");
        assert_eq!(m[n].exclusive, Some(0), "{n}");
    }
    assert!(!run.report.sharing_exact);
    // The counted bytes are still the bytes on disk.
    assert_eq!(run.snapshot.root().unwrap().allocated(), 8192);
}

#[test]
fn a_nested_subvolume_that_cannot_be_read_is_an_unreadable_directory() {
    let mut fs = MemFs::new();
    fs.plain(FS, ROOT, "fine", 4096);
    fs.subvolume(FS, ROOT, "broken", 300);
    fs.plain(300, ROOT, "lost.bin", 1 << 20);
    fs.subvolume(FS, ROOT, "ok", 301);
    fs.plain(301, ROOT, "kept.bin", 8192);
    let mut src = fs.source();
    src.fail_trees.insert(300);
    let run = read(&src);
    let m = run.flat();
    assert_eq!(m["broken"].kind, NodeKind::Dir);
    assert!(m["broken"].failed);
    assert!(!m.contains_key("broken/lost.bin"));
    assert_eq!(m["ok/kept.bin"].allocated, 8192);
    let root = run.snapshot.root().unwrap();
    assert!(root.has_unreadable_below());
    assert!(root.bytes_are_lower_bound());
    assert_eq!(run.report.dropped_trees, 1);
    // What the lost subvolume refers to is not known: nothing is claimed to be
    // used by one file alone, and the report says the figure is not exact.
    assert!(!run.report.sharing_exact);
    assert!(m["fine"].flags.contains(NodeFlags::SHARED));
    assert_eq!(m["fine"].exclusive, Some(0));
    assert_eq!(
        m["fine"].allocated, 4096,
        "the bytes themselves are still counted"
    );
    assert_eq!(
        run.report
            .diagnostics
            .count(ProblemKind::SubvolumeUnreadable),
        1
    );
}

#[test]
fn a_tree_that_fails_halfway_is_dropped_whole() {
    let mut fs = MemFs::new();
    fs.plain(FS, ROOT, "fine", 4096);
    fs.subvolume(FS, ROOT, "half", 300);
    for i in 0..20 {
        fs.plain(300, ROOT, &format!("f{i}"), 4096);
    }
    let src = fs.source();
    // The start tree has three items and one entry; let the nested tree
    // deliver some of its items and then fail.
    *src.fail_after_items.lock().unwrap() = None;
    let counted = src
        .fs
        .items
        .range((FS, bs_scan_btrfs::Key::MIN)..)
        .take_while(|((t, _), _)| *t == FS)
        .count();
    *src.fail_after_items.lock().unwrap() = Some(counted as u64 + 10);
    let run = read(&src);
    let m = run.flat();
    assert!(m["half"].failed);
    assert!(
        !m.keys().any(|k| k.starts_with("half/")),
        "nothing of a half-read tree is kept"
    );
    assert_eq!(m["fine"].allocated, 4096);
    assert_eq!(run.snapshot.root().unwrap().allocated(), 4096);
    // And nothing of it is left in the tables to be counted as strays.
    assert_eq!(
        run.report.inodes, 2,
        "the start's root directory and `fine`"
    );
    assert_eq!(run.report.unreachable.inodes, 0);
}

#[test]
fn a_start_tree_that_fails_is_an_error_and_not_a_tree() {
    let mut fs = MemFs::new();
    fs.plain(FS, ROOT, "x", 1);
    let mut src = fs.source();
    src.fail_trees.insert(FS);
    let error = try_read(&src, START, &BtrfsOptions::default())
        .err()
        .expect("an error");
    assert!(matches!(error, BtrfsError::Io(_)), "{error}");
}

#[test]
fn the_start_must_be_a_directory_of_its_tree() {
    let mut fs = MemFs::new();
    let f = fs.plain(FS, ROOT, "x", 1);
    let src = fs.source();
    for inode in [f, 9999, 0] {
        let error = try_read(&src, Start { tree: FS, inode }, &BtrfsOptions::default())
            .err()
            .expect("an error");
        assert!(matches!(error, BtrfsError::BadRoot(_)), "{inode}: {error}");
    }
}

#[test]
fn starting_below_the_root_covers_that_directory_only() {
    let mut fs = MemFs::new();
    let a = fs.mkdir(FS, ROOT, "a");
    fs.plain(FS, a, "in-a", 4096);
    fs.plain(FS, ROOT, "outside", 8192);
    let run = try_read(
        &fs.source(),
        Start { tree: FS, inode: a },
        &BtrfsOptions::default(),
    )
    .unwrap();
    let m = run.flat();
    assert_eq!(
        m.keys().cloned().collect::<Vec<_>>(),
        vec!["in-a".to_string()]
    );
    assert_eq!(
        run.report.unreachable.inodes, 2,
        "the root directory and `outside`"
    );
    assert_eq!(run.report.unreachable.allocated, 8192);
}

#[test]
fn an_inode_nobody_names_is_counted_as_unreachable() {
    let mut fs = MemFs::new();
    fs.plain(FS, ROOT, "named", 4096);
    // A file whose last name was removed while it was open.
    let addr = fs.fresh_extent(65536);
    fs.items.insert(
        (
            FS,
            bs_scan_btrfs::Key {
                objectid: 700,
                ty: 1,
                offset: 0,
            },
        ),
        common::memsrc::inode_item(65536, 0, common::memsrc::S_IFREG | 0o600, 0),
    );
    fs.items.insert(
        (
            FS,
            bs_scan_btrfs::Key {
                objectid: 700,
                ty: 108,
                offset: 0,
            },
        ),
        {
            let mut b = vec![0u8; 53];
            b[20] = 1;
            b[21..29].copy_from_slice(&addr.to_le_bytes());
            b[29..37].copy_from_slice(&65536u64.to_le_bytes());
            b
        },
    );
    let run = read(&fs.source());
    assert_eq!(run.report.unreachable.inodes, 1);
    assert_eq!(run.report.unreachable.allocated, 65536);
    assert_eq!(run.snapshot.root().unwrap().allocated(), 4096);
}

#[test]
fn an_entry_for_an_inode_that_is_gone_is_skipped_and_counted_but_does_not_mark_the_directory() {
    let mut fs = MemFs::new();
    fs.plain(FS, ROOT, "here", 4096);
    let ghost = fs.plain(FS, ROOT, "ghost", 4096);
    fs.items.remove(&(
        FS,
        bs_scan_btrfs::Key {
            objectid: ghost,
            ty: 1,
            offset: 0,
        },
    ));
    fs.items.remove(&(
        FS,
        bs_scan_btrfs::Key {
            objectid: ghost,
            ty: 108,
            offset: 0,
        },
    ));
    let run = read(&fs.source());
    let m = run.flat();
    assert!(m.contains_key("here") && !m.contains_key("ghost"));
    assert_eq!(run.report.vanished_entries, 1);
    assert_eq!(
        run.report
            .diagnostics
            .count(ProblemKind::DirentToMissingInode),
        1
    );
    assert!(run.snapshot.root().unwrap().is_complete());
}

#[test]
fn an_entry_that_contradicts_its_inode_marks_its_directory() {
    let mut fs = MemFs::new();
    let d = fs.mkdir(FS, ROOT, "d");
    fs.plain(FS, d, "fine", 4096);
    let victim = fs.plain(FS, d, "victim", 4096);
    // The recorded type says directory; the inode is a regular file.
    let key = bs_scan_btrfs::Key {
        objectid: d,
        ty: 96,
        offset: 3,
    };
    let item = fs.items.get_mut(&(FS, key)).unwrap();
    item[29] = 2;
    let _ = victim;
    let run = read(&fs.source());
    let m = run.flat();
    assert!(!m.contains_key("d/victim"));
    assert!(m["d"].failed, "its bytes are missing from the total");
    assert_eq!(
        run.report
            .diagnostics
            .count(ProblemKind::DirentTypeMismatch),
        1
    );
    assert!(run.snapshot.root().unwrap().bytes_are_lower_bound());
}

#[test]
fn names_that_cannot_exist_are_dropped_and_mark_the_directory() {
    let mut fs = MemFs::new();
    fs.plain(FS, ROOT, "ok", 4096);
    fs.plain(FS, ROOT, "a/b", 4096);
    fs.plain(FS, ROOT, "x\0y", 4096);
    fs.plain(FS, ROOT, "", 4096);
    let run = read(&fs.source());
    let m = run.flat();
    assert_eq!(m.len(), 1);
    assert_eq!(run.report.diagnostics.count(ProblemKind::DirentBadName), 3);
    assert!(run.snapshot.root().unwrap().is_unreadable());
}

#[test]
fn a_directory_entered_twice_is_entered_once() {
    let mut fs = MemFs::new();
    let a = fs.mkdir(FS, ROOT, "a");
    fs.plain(FS, a, "inside", 4096);
    // A second entry for the same directory, and one that loops to the root.
    fs.link_dir(FS, ROOT, "again", a);
    let run = read(&fs.source());
    let m = run.flat();
    let present = ["a", "again"]
        .iter()
        .filter(|n| m.contains_key(**n))
        .count();
    assert_eq!(present, 1);
    assert_eq!(m.keys().filter(|k| k.ends_with("inside")).count(), 1);
    assert_eq!(
        run.report
            .diagnostics
            .count(ProblemKind::DirectoryLinkedTwice),
        1
    );
    assert!(run.snapshot.root().unwrap().is_unreadable());
}

#[test]
fn a_loop_does_not_run_forever() {
    let mut fs = MemFs::new();
    let a = fs.mkdir(FS, ROOT, "a");
    let b = fs.mkdir(FS, a, "b");
    // `b` has an entry back to `a`, and `a` one back to the root.
    fs.link_dir(FS, b, "up", a);
    fs.link_dir(FS, a, "root", ROOT);
    let run = read(&fs.source());
    assert_eq!(
        run.report
            .diagnostics
            .count(ProblemKind::DirectoryLinkedTwice),
        2
    );
    assert_eq!(run.flat().len(), 2);
}

#[test]
fn conflicting_extent_sizes_mark_the_file() {
    let mut fs = MemFs::new();
    let addr = fs.fresh_extent(8192);
    fs.file(FS, ROOT, b"a", 8192, &[Ext::Disk(addr, 8192, false)]);
    fs.file(FS, ROOT, b"b", 8192, &[Ext::Disk(addr, 4096, false)]);
    let run = read(&fs.source());
    let m = run.flat();
    assert!(m["b"].failed);
    assert!(!m["a"].failed);
    assert_eq!(
        run.report
            .diagnostics
            .count(ProblemKind::ExtentSizeMismatch),
        1
    );
    assert_eq!(
        m["b"].allocated, 0,
        "the disputed extent is counted at the first file"
    );
}

#[test]
fn malformed_items_mark_what_they_belong_to() {
    let mut fs = MemFs::new();
    let f = fs.plain(FS, ROOT, "file", 4096);
    let g = fs.plain(FS, ROOT, "inode-short", 4096);
    let h = fs.mkdir(FS, ROOT, "dir");
    fs.plain(FS, h, "child", 4096);
    // A file extent item that is too short.
    let ext = bs_scan_btrfs::Key {
        objectid: f,
        ty: 108,
        offset: 0,
    };
    fs.items.get_mut(&(FS, ext)).unwrap().truncate(30);
    // An inode item that is too short.
    let ino = bs_scan_btrfs::Key {
        objectid: g,
        ty: 1,
        offset: 0,
    };
    fs.items.get_mut(&(FS, ino)).unwrap().truncate(100);
    // A directory item cut off in the middle of its name.
    let idx = bs_scan_btrfs::Key {
        objectid: h,
        ty: 96,
        offset: 2,
    };
    fs.items.get_mut(&(FS, idx)).unwrap().truncate(33);
    let run = read(&fs.source());
    let m = run.flat();
    assert!(m["file"].failed);
    assert!(m["inode-short"].failed);
    assert!(m["dir"].failed);
    assert!(run.report.diagnostics.count(ProblemKind::ItemMalformed) >= 3);
    assert!(run.snapshot.root().unwrap().bytes_are_lower_bound());
}

#[test]
fn an_inode_with_an_impossible_type_is_unreadable() {
    let mut fs = MemFs::new();
    let f = fs.plain(FS, ROOT, "weird", 4096);
    let ino = bs_scan_btrfs::Key {
        objectid: f,
        ty: 1,
        offset: 0,
    };
    fs.items.get_mut(&(FS, ino)).unwrap()[52..56].copy_from_slice(&0o030_000u32.to_le_bytes());
    let run = read(&fs.source());
    let m = run.flat();
    // Shown, as the nearest thing to what it is, and flagged.
    assert!(m["weird"].failed);
    assert_eq!(m["weird"].kind, NodeKind::Other);
    assert_eq!(run.report.diagnostics.count(ProblemKind::InodeInvalid), 1);
    assert!(run.snapshot.root().unwrap().has_unreadable_below());
}

#[test]
fn items_for_an_inode_with_no_inode_item_are_ignored() {
    let mut fs = MemFs::new();
    fs.plain(FS, ROOT, "ok", 4096);
    fs.items.insert(
        (
            FS,
            bs_scan_btrfs::Key {
                objectid: 900,
                ty: 108,
                offset: 0,
            },
        ),
        vec![0u8; 53],
    );
    fs.items.insert(
        (
            FS,
            bs_scan_btrfs::Key {
                objectid: 900,
                ty: 96,
                offset: 2,
            },
        ),
        common::memsrc::dir_item(5, 1, 1, b"x"),
    );
    let run = read(&fs.source());
    assert_eq!(
        run.report.diagnostics.count(ProblemKind::ItemWithoutInode),
        2
    );
    assert_eq!(run.flat().len(), 1);
}

#[test]
fn the_memory_limit_refuses_what_it_cannot_hold() {
    let mut fs = MemFs::new();
    for i in 0..500 {
        fs.plain(FS, ROOT, &format!("file-{i}"), 4096);
    }
    let src = fs.source();
    for limit in [0, 1000, 10_000, 100_000] {
        let options = BtrfsOptions {
            max_memory_bytes: limit,
            ..BtrfsOptions::default()
        };
        let error = try_read(&src, START, &options).err().expect("refused");
        assert!(matches!(error, BtrfsError::TooLarge(_)), "{limit}: {error}");
    }
    let options = BtrfsOptions {
        max_memory_bytes: 64 << 20,
        ..BtrfsOptions::default()
    };
    let run = try_read(&src, START, &options).unwrap();
    assert!(run.report.memory_used_bytes > 0);
    assert!(run.report.memory_used_bytes < 64 << 20);
}

#[test]
fn a_very_deep_tree_is_cut_off_and_marked() {
    let mut fs = MemFs::new();
    let mut at = ROOT;
    for _ in 0..66_000 {
        at = fs.mkdir(FS, at, "d");
    }
    fs.plain(FS, at, "bottom", 4096);
    let run = read(&fs.source());
    assert!(run.report.diagnostics.count(ProblemKind::TooDeep) >= 1);
    assert!(run.snapshot.root().unwrap().bytes_are_lower_bound());
}

#[test]
fn cancelling_keeps_the_partial_tree_marked_incomplete() {
    let mut fs = MemFs::new();
    for i in 0..50 {
        fs.plain(FS, ROOT, &format!("f{i}"), 4096);
    }
    let src = fs.source();
    let (ctx, rx) = ScanContext::channel(ScanOptions::default(), 1 << 16);
    ctx.cancel();
    let (end, report) =
        read_filesystem(&src, START, b"/mnt", &BtrfsOptions::default(), &ctx).unwrap();
    assert_eq!(end, ScanEnd::Cancelled);
    assert!(report.cancelled);
    drop(ctx);
    let mut agg = bs_tree::Aggregator::new();
    for b in rx.try_iter() {
        agg.apply(&b);
    }
    agg.mark_stopped();
    agg.finish();
    let snap = agg.snapshot();
    snap.verify().unwrap();
    assert!(!snap.scan_complete());
}

#[test]
fn files_are_colored_by_extension() {
    let mut fs = MemFs::new();
    fs.plain(FS, ROOT, "clip.mp4", 4096);
    fs.plain(FS, ROOT, "README", 4096);
    let src = fs.source();
    let run = read(&src);
    let m = run.flat();
    assert_ne!(m["clip.mp4"].category, 0);
    assert_eq!(m["README"].category, 0);
    // Switched off, nothing is classified.
    let (ctx, rx) = ScanContext::channel(
        ScanOptions {
            categories: false,
            ..ScanOptions::default()
        },
        1 << 16,
    );
    read_filesystem(&src, START, b"/mnt", &BtrfsOptions::default(), &ctx).unwrap();
    drop(ctx);
    let mut agg = bs_tree::Aggregator::new();
    for b in rx.try_iter() {
        agg.apply(&b);
    }
    agg.finish();
    let flat = common::flat(&agg.snapshot());
    assert_eq!(flat["clip.mp4"].category, 0);
}

#[test]
fn a_reader_with_the_same_input_gives_the_same_output() {
    let build = || {
        let mut fs = MemFs::new();
        for i in 0..40 {
            let d = fs.mkdir(FS, ROOT, &format!("d{i}"));
            for j in 0..10 {
                fs.plain(FS, d, &format!("f{j}"), 4096 * (j + 1));
            }
        }
        fs
    };
    let a = read(&build().source()).flat();
    let b = read(&build().source()).flat();
    let strip = |m: &std::collections::BTreeMap<String, common::Info>| {
        m.iter()
            .map(|(k, v)| (k.clone(), v.logical, v.allocated, v.mtime, v.flags))
            .collect::<Vec<_>>()
    };
    assert_eq!(strip(&a), strip(&b));
}
