//! Snapshots and the subvolumes nested in their origin.
//!
//! A snapshot is a copy of a subvolume's tree, so it keeps the directory
//! entries of the subvolumes nested in the original, but the kernel does not
//! nest those subvolumes in the snapshot: the root tree has a root reference
//! (keyed by the parent tree, the reference type and the child) only for the
//! place a subvolume really is, and where there is none the kernel shows an
//! empty directory (`fixup_tree_root_location` and `new_simple_dir`,
//! `fs/btrfs/inode.c`). The reader must do the same and must not read the
//! original's subvolume a second time, as that would make the snapshot's
//! folder look broken and the nested subvolume vanish from where it is.
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
#[path = "common/diff.rs"]
mod diff;
#[path = "common/exceptions.rs"]
mod exceptions;

use bs_scan_btrfs::consts::{ROOT_REF, ROOT_TREE_ID};
use bs_scan_btrfs::{BtrfsOptions, Key, ProblemKind, Start};
use bs_tree::NodeKind;
use common::memsrc::{MemFs, MemSource};
use common::{START, read, try_read};

const FS: u64 = 5;
const ROOT: u64 = 256;
const SUB1: u64 = 256;
const INNER: u64 = 257;
const SNAP: u64 = 258;
const SNAP_RO: u64 = 259;

/// The live fixture's shape: `sub1` with a nested subvolume `inner`, and two
/// snapshots of `sub1` taken after that.
fn fixture() -> MemFs {
    let mut fs = MemFs::new();
    fs.subvolume(FS, ROOT, "sub1", SUB1);
    fs.plain(SUB1, ROOT, "data.bin", 1 << 20);
    fs.subvolume(SUB1, ROOT, "inner", INNER);
    fs.plain(INNER, ROOT, "inner.bin", 512 << 10);
    fs.snapshot(SUB1, FS, ROOT, "snap1", SNAP);
    fs.snapshot(SUB1, FS, ROOT, "snap1-ro", SNAP_RO);
    fs.plain(SNAP, ROOT, "only-in-snapshot.bin", 64 << 10);
    fs
}

#[test]
fn the_snapshot_of_a_subvolume_with_a_nested_one_shows_an_empty_folder_there() {
    let fs = fixture();
    // The premise, as the kernel leaves it: the snapshot's directory has the
    // entry, the root tree has no reference for it.
    assert!(fs.items.contains_key(&(
        ROOT_TREE_ID,
        Key {
            objectid: SUB1,
            ty: ROOT_REF,
            offset: INNER
        }
    )));
    for snap in [SNAP, SNAP_RO] {
        assert!(!fs.items.contains_key(&(
            ROOT_TREE_ID,
            Key {
                objectid: snap,
                ty: ROOT_REF,
                offset: INNER
            }
        )));
    }
    let run = read(&fs.source());
    let m = run.flat();

    // The snapshots are whole, and not marked unreadable.
    for p in ["snap1", "snap1-ro"] {
        assert!(!m[p].failed, "{p} is a readable folder");
    }
    // Where the original has the nested subvolume, the snapshot has an empty,
    // complete folder with no time of its own.
    for p in ["snap1/inner", "snap1-ro/inner"] {
        assert_eq!(m[p].kind, NodeKind::Dir, "{p}");
        assert!(!m[p].failed, "{p}");
        assert_eq!(m[p].mtime, 0, "{p}");
        assert!(
            !m.keys().any(|k| k.starts_with(&format!("{p}/"))),
            "{p} holds nothing"
        );
    }
    assert_eq!(run.report.stand_in_dirs, 2);
    // The nested subvolume is where it is nested, once.
    assert_eq!(m["sub1/inner/inner.bin"].allocated, 512 << 10);
    assert!(!m["sub1/inner"].failed);
    assert_eq!(m["sub1/inner"].mtime, common::memsrc::MTIME);
    // Nothing was refused, skipped or called inconsistent.
    assert_eq!(
        run.report.diagnostics.total(),
        0,
        "{:?}",
        run.report.diagnostics
    );
    assert_eq!(run.report.unreadable_entries, 0);
    assert!(run.snapshot.scan_complete());
    let root = run.snapshot.root().unwrap();
    assert!(!root.has_unreadable_below());
    assert!(!root.bytes_are_lower_bound());
    // The trees read: the top level, sub1, inner and the two snapshots.
    assert_eq!(run.report.trees, 5);
    // Counted once: sub1's data and the snapshots' copies are one extent, and
    // so is inner's file; the snapshot adds its own file.
    assert_eq!(
        root.allocated(),
        (1 << 20) + (512 << 10) + (64 << 10),
        "every extent once"
    );
    assert!(run.report.sharing_exact);
}

#[test]
fn a_walk_order_that_meets_the_snapshot_first_gives_the_same_answer() {
    // The snapshot is listed before its origin, so the origin's nested
    // subvolume must not be taken by the snapshot's stale entry.
    let mut fs = MemFs::new();
    fs.make_tree(SUB1);
    fs.plain(SUB1, ROOT, "data.bin", 1 << 20);
    fs.subvolume(SUB1, ROOT, "inner", INNER);
    fs.plain(INNER, ROOT, "inner.bin", 512 << 10);
    fs.snapshot(SUB1, FS, ROOT, "a-snap", SNAP);
    // Only now is the origin put in the top level, after the snapshot.
    fs.subvolume(FS, ROOT, "z-sub1", SUB1);
    let run = read(&fs.source());
    let m = run.flat();
    assert!(!m["a-snap"].failed);
    assert_eq!(m["a-snap/inner"].mtime, 0);
    assert_eq!(m["z-sub1/inner/inner.bin"].allocated, 512 << 10);
    assert_eq!(run.report.stand_in_dirs, 1);
    assert_eq!(run.report.diagnostics.total(), 0);
}

#[test]
fn a_scan_that_starts_in_the_snapshot_does_not_read_the_origins_nested_subvolume() {
    let fs = fixture();
    let src = fs.source();
    let start = Start {
        tree: SNAP,
        inode: ROOT,
    };
    let run = try_read(&src, start, &BtrfsOptions::default()).unwrap();
    let m = run.flat();
    assert_eq!(m["inner"].kind, NodeKind::Dir);
    assert_eq!(m["inner"].mtime, 0);
    assert!(!m.keys().any(|k| k.starts_with("inner/")));
    assert_eq!(run.report.stand_in_dirs, 1);
    // Only the snapshot is in the scan; the nested subvolume is another
    // subvolume of the file system that the sharing census looks at.
    assert_eq!(run.report.trees, 1);
    assert!(run.report.census_trees >= 1);
    assert!(run.report.sharing_exact);
    assert_eq!(run.report.diagnostics.total(), 0);
    assert!(run.snapshot.scan_complete());
}

/// A top level with `sub1` (a subvolume) holding the entry `inner` for the
/// subvolume `INNER`, and `reference` as the root reference that says where
/// `INNER` is.
fn with_reference(reference: impl FnOnce(&mut MemFs)) -> MemSource {
    let mut fs = MemFs::new();
    fs.subvolume(FS, ROOT, "sub1", SUB1);
    fs.plain(SUB1, ROOT, "data.bin", 4096);
    fs.make_tree(INNER);
    fs.plain(INNER, ROOT, "inner.bin", 8192);
    fs.entry_of_unnested_subvolume(SUB1, ROOT, "inner", INNER);
    reference(&mut fs);
    fs.source()
}

/// A way to leave (or not leave) a root reference for `INNER`.
type Reference = Box<dyn FnOnce(&mut MemFs)>;

#[test]
fn a_reference_must_name_this_directory_and_this_name() {
    // The right directory and name: nested.
    let nested = with_reference(|fs| fs.root_ref(SUB1, ROOT, b"inner", INNER));
    let run = read(&nested);
    assert_eq!(run.report.stand_in_dirs, 0);
    assert_eq!(run.flat()["sub1/inner/inner.bin"].allocated, 8192);

    // No reference, one for another parent, another directory, another name,
    // a longer name: each leaves an empty folder, and nothing of the
    // subvolume is read into the tree.
    let cases: [(&str, Reference); 5] = [
        ("none", Box::new(|_| {})),
        (
            "another parent",
            Box::new(|fs| fs.root_ref(FS, ROOT, b"inner", INNER)),
        ),
        (
            "another directory",
            Box::new(|fs| fs.root_ref(SUB1, ROOT + 7, b"inner", INNER)),
        ),
        (
            "another name",
            Box::new(|fs| fs.root_ref(SUB1, ROOT, b"innex", INNER)),
        ),
        (
            "a longer name",
            Box::new(|fs| fs.root_ref(SUB1, ROOT, b"inner2", INNER)),
        ),
    ];
    for (what, reference) in cases {
        let run = read(&with_reference(reference));
        let m = run.flat();
        assert_eq!(run.report.stand_in_dirs, 1, "{what}");
        assert_eq!(m["sub1/inner"].kind, NodeKind::Dir, "{what}");
        assert!(!m["sub1/inner"].failed, "{what}");
        assert!(!m.contains_key("sub1/inner/inner.bin"), "{what}");
        assert_eq!(run.report.trees, 2, "{what}: the top level and sub1");
        assert_eq!(run.report.diagnostics.total(), 0, "{what}");
        assert!(run.snapshot.scan_complete(), "{what}");
    }
}

#[test]
fn damaged_reference_items_are_unreadable_never_a_complete_empty_folder() {
    // A reference cut short at every length, or one that breaks the kernel's
    // rules for the item (a name of 0 or over 255 bytes, a size that is not
    // the header plus the name), is damage. The entry is not placed: the
    // folder is unreadable and noted, nothing of the subvolume is read into
    // the tree, and nothing above claims to be whole.
    let mut good = Vec::new();
    good.extend_from_slice(&ROOT.to_le_bytes());
    good.extend_from_slice(&1u64.to_le_bytes());
    good.extend_from_slice(&5u16.to_le_bytes());
    good.extend_from_slice(b"inner");
    let mut damaged: Vec<(String, Vec<u8>)> = (0..good.len())
        .map(|cut| (format!("cut at {cut}"), good[..cut].to_vec()))
        .collect();
    let mut with_extra = good.clone();
    with_extra.extend_from_slice(b"junk");
    damaged.push(("bytes after the name".into(), with_extra));
    let mut empty_name = good[..18].to_vec();
    empty_name[16..18].copy_from_slice(&0u16.to_le_bytes());
    damaged.push(("empty name".into(), empty_name));
    let mut long_name = good[..18].to_vec();
    long_name[16..18].copy_from_slice(&256u16.to_le_bytes());
    long_name.extend_from_slice(&[b'a'; 256]);
    damaged.push(("name over 255 bytes".into(), long_name));
    let mut name_past_the_end = good.clone();
    name_past_the_end[16..18].copy_from_slice(&200u16.to_le_bytes());
    damaged.push(("name running past the item".into(), name_past_the_end));
    for (what, item) in damaged {
        let src = with_reference(|fs| {
            fs.items.insert(
                (
                    ROOT_TREE_ID,
                    Key {
                        objectid: SUB1,
                        ty: ROOT_REF,
                        offset: INNER,
                    },
                ),
                item,
            );
        });
        let run = read(&src);
        let m = run.flat();
        assert_eq!(run.report.stand_in_dirs, 0, "{what}");
        assert!(m["sub1/inner"].failed, "{what}");
        assert!(!m.contains_key("sub1/inner/inner.bin"), "{what}");
        assert!(
            run.report.diagnostics.count(ProblemKind::ItemMalformed) >= 1,
            "{what}: {:?}",
            run.report.diagnostics
        );
        assert!(!run.report.sharing_exact, "{what}");
        assert!(run.report.unreadable_entries >= 1, "{what}");
        assert!(
            run.snapshot.root().unwrap().has_unreadable_below(),
            "{what}"
        );
        assert!(
            run.snapshot.root().unwrap().bytes_are_lower_bound(),
            "{what}"
        );
    }
}

#[test]
fn a_reference_cut_to_its_header_does_not_hide_an_eight_mebibyte_subvolume() {
    // Eight mebibytes in a nested subvolume whose reference is cut to its
    // 18-byte header: the folder is unreadable and noted, and the root says
    // its bytes are a lower bound.
    let mut fs = MemFs::new();
    fs.subvolume(5, 256, "sub1", 256);
    fs.plain(256, 256, "data.bin", 4096);
    fs.make_tree(257);
    fs.plain(257, 256, "inner.bin", 8 << 20);
    fs.entry_of_unnested_subvolume(256, 256, "inner", 257);
    let mut item = Vec::new();
    item.extend_from_slice(&256u64.to_le_bytes());
    item.extend_from_slice(&1u64.to_le_bytes());
    item.extend_from_slice(&5u16.to_le_bytes());
    fs.items.insert(
        (
            ROOT_TREE_ID,
            Key {
                objectid: 256,
                ty: ROOT_REF,
                offset: 257,
            },
        ),
        item,
    );
    let run = read(&fs.source());
    let m = run.flat();
    eprintln!(
        "stand_in_dirs={} diag_total={} complete={} sub1/inner failed={} root alloc={}",
        run.report.stand_in_dirs,
        run.report.diagnostics.total(),
        run.snapshot.scan_complete(),
        m["sub1/inner"].failed,
        run.snapshot.root().unwrap().allocated()
    );
    assert_eq!(run.report.stand_in_dirs, 0);
    assert!(run.report.diagnostics.total() > 0);
    // The tree calls a folder complete once it is counted; an unreadable
    // entry below it is what says its bytes are only a lower bound.
    let root = run.snapshot.root().unwrap();
    assert!(root.has_unreadable_below());
    assert!(root.bytes_are_lower_bound());
    assert!(m["sub1/inner"].failed);
}

#[test]
fn a_root_tree_that_cannot_be_searched_leaves_the_entry_unplaced_and_the_sharing_unknown() {
    let mut fs = MemFs::new();
    fs.plain(FS, ROOT, "fine", 4096);
    fs.subvolume(FS, ROOT, "nested", SUB1);
    fs.plain(SUB1, ROOT, "kept.bin", 8192);
    let mut src = fs.source();
    src.fail_trees.insert(ROOT_TREE_ID);
    let run = try_read(&src, START, &BtrfsOptions::default()).unwrap();
    let m = run.flat();
    // Not placed is not the same as empty: the folder is unreadable, so no
    // total above it claims to be whole.
    assert!(m["nested"].failed);
    assert!(!m.contains_key("nested/kept.bin"));
    assert!(run.snapshot.root().unwrap().has_unreadable_below());
    assert!(run.snapshot.root().unwrap().bytes_are_lower_bound());
    assert!(!run.report.sharing_exact);
    assert!(run.report.diagnostics.count(ProblemKind::SubvolumeSkipped) >= 1);
    assert_eq!(run.report.stand_in_dirs, 0);
}

#[test]
fn the_root_tree_is_searched_once_for_each_nested_entry() {
    // Placing is cheap: one key looked up for each subvolume entry, and none
    // for a tree with no subvolumes.
    let mut fs = MemFs::new();
    fs.plain(FS, ROOT, "only", 4096);
    let src = fs.source();
    let _ = read(&src);
    let before = src
        .searches
        .lock()
        .unwrap()
        .iter()
        .filter(|t| **t == ROOT_TREE_ID)
        .count();
    // The census lists the root tree once; nothing more for a tree without
    // subvolume entries (it has no extents to look for either).
    assert_eq!(before, 1);

    let src = fixture().source();
    let _ = read(&src);
    let lookups = src
        .searches
        .lock()
        .unwrap()
        .iter()
        .filter(|t| **t == ROOT_TREE_ID)
        .count();
    // Entries naming subvolumes: sub1, snap1, snap1-ro in the top level;
    // inner in sub1, and its copies in the two snapshots; plus the census.
    assert_eq!(lookups, 6 + 1);
}

#[test]
fn the_report_of_what_was_compared_names_each_class_of_path() {
    // The live comparison's table, run on the reader against itself: every
    // path of the fixture lands in a class, and the folders the kernel makes
    // up are named as such.
    let run = read(&fixture().source());
    let t = diff::tree_of(&run.snapshot);
    let outcome = diff::compare(&t, &t, &[]);
    assert!(outcome.is_clean());
    let table = exceptions::class_table(&t, &t, &outcome);
    for class in [
        "subvolume or snapshot root",
        "stand-in folder (snapshot of a nested subvolume)",
        "plain file",
        "file sharing extents",
    ] {
        assert!(table.contains(class), "{class}: {table}");
    }
    assert!(!table.contains("unknown"), "{table}");
    let row = |class: &str| {
        table
            .lines()
            .find(|l| l.contains(class))
            .map(|l| l.split_whitespace().rev().take(3).collect::<Vec<_>>())
            .unwrap()
    };
    // Two stand-ins, four roots; nothing is unexplained.
    assert_eq!(row("stand-in folder")[2], "2");
    assert_eq!(row("subvolume or snapshot root")[2], "4");
    assert_eq!(row("stand-in folder")[0], "0");
    assert!(exceptions::STAND_INS.iter().all(|p| t.contains_key(*p)));
}
