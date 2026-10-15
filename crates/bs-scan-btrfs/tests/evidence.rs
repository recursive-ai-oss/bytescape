//! Sharing evidence that cannot be read: an extent item that does not parse
//! is still a reference to something, so no file may be called the only user
//! of an extent on the strength of items it could not read.
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

use bs_scan_btrfs::ProblemKind;
use bs_scan_btrfs::consts::EXTENT_DATA;
use bs_tree::NodeFlags;
use common::memsrc::{Ext, MemFs, MemSource};
use common::{Run, read};

const FS: u64 = 5;
const ROOT: u64 = 256;
const SNAPSHOT: u64 = 400;

/// Cuts every extent item of `tree` (or of one inode, when given) to `len`
/// bytes.
fn cut(fs: &mut MemFs, tree: u64, ino: Option<u64>, len: usize) {
    for ((t, key), data) in fs.items.iter_mut() {
        if *t == tree && key.ty == EXTENT_DATA && ino.is_none_or(|i| i == key.objectid) {
            data.truncate(len);
        }
    }
}

/// `a` in the scan, and a copy of it in a snapshot the scan does not enter.
fn with_snapshot_copy() -> (MemFs, u64) {
    let mut fs = MemFs::new();
    let a = fs.fresh_extent(65536);
    fs.file(FS, ROOT, b"a", 65536, &[Ext::Disk(a, 65536, false)]);
    fs.plain(FS, ROOT, "b", 4096);
    fs.make_tree(SNAPSHOT);
    fs.file(
        SNAPSHOT,
        ROOT,
        b"copy-of-a",
        65536,
        &[Ext::Disk(a, 65536, false)],
    );
    (fs, a)
}

fn everything_is_taken_as_shared(run: &Run, names: &[&str]) {
    let m = run.flat();
    for n in names {
        assert!(m[*n].flags.contains(NodeFlags::SHARED), "{n} looks private");
        assert_eq!(m[*n].exclusive, Some(0), "{n}");
        let est = run.snapshot.delete_would_free(m[*n].id).unwrap();
        assert_eq!(est.at_least, 0, "{n}");
    }
    assert!(!run.report.sharing_exact);
}

#[test]
fn a_snapshot_item_cut_short_but_still_naming_its_extent_makes_that_extent_shared() {
    let (mut fs, _) = with_snapshot_copy();
    // The disk address (bytes 21..29) is intact; the rest of the item is not.
    cut(&mut fs, SNAPSHOT, None, 40);
    let run = read(&fs.source());
    let m = run.flat();
    assert!(m["a"].flags.contains(NodeFlags::SHARED));
    assert_eq!(m["a"].exclusive, Some(0));
    let est = run.snapshot.delete_would_free(m["a"].id).unwrap();
    assert_eq!((est.at_least, est.at_most), (0, 65536));
    // Nothing else is dragged in, and the damage is on record.
    assert!(!m["b"].flags.contains(NodeFlags::SHARED));
    assert_eq!(run.report.diagnostics.count(ProblemKind::ItemMalformed), 1);
}

#[test]
fn a_snapshot_item_that_names_nothing_readable_makes_everything_shared() {
    // Too short to hold the address, too short to know its type, and a type
    // that does not exist.
    for len in [28usize, 25, 20, 3, 0] {
        let (mut fs, _) = with_snapshot_copy();
        cut(&mut fs, SNAPSHOT, None, len);
        let run = read(&fs.source());
        everything_is_taken_as_shared(&run, &["a", "b"]);
    }
    let (mut fs, _) = with_snapshot_copy();
    for ((t, key), data) in fs.items.iter_mut() {
        if *t == SNAPSHOT && key.ty == EXTENT_DATA {
            data[20] = 9;
        }
    }
    let run = read(&fs.source());
    everything_is_taken_as_shared(&run, &["a", "b"]);
}

#[test]
fn an_inline_item_in_a_snapshot_is_not_a_reference_to_disk() {
    let mut fs = MemFs::new();
    fs.plain(FS, ROOT, "a", 4096);
    fs.make_tree(SNAPSHOT);
    fs.symlink(SNAPSHOT, ROOT, "link", "somewhere");
    let run = read(&fs.source());
    let m = run.flat();
    assert!(!m["a"].flags.contains(NodeFlags::SHARED));
    assert!(run.report.sharing_exact);
}

#[test]
fn a_damaged_item_in_the_scan_shares_the_extent_it_names_in_either_order() {
    for damaged_first in [false, true] {
        let mut fs = MemFs::new();
        let x = fs.fresh_extent(65536);
        let (bad, good) = if damaged_first {
            let bad = fs.file(FS, ROOT, b"bad", 65536, &[Ext::Disk(x, 65536, false)]);
            let good = fs.file(FS, ROOT, b"good", 65536, &[Ext::Disk(x, 65536, false)]);
            (bad, good)
        } else {
            let good = fs.file(FS, ROOT, b"good", 65536, &[Ext::Disk(x, 65536, false)]);
            let bad = fs.file(FS, ROOT, b"bad", 65536, &[Ext::Disk(x, 65536, false)]);
            (bad, good)
        };
        let _ = good;
        fs.plain(FS, ROOT, "other", 4096);
        cut(&mut fs, FS, Some(bad), 40);
        let run = read(&fs.source());
        let m = run.flat();
        assert!(
            m["good"].flags.contains(NodeFlags::SHARED),
            "{damaged_first}"
        );
        assert_eq!(m["good"].exclusive, Some(0), "{damaged_first}");
        // The file with the damaged item is marked, not trusted.
        assert!(m["bad"].failed, "{damaged_first}");
        assert!(!m["other"].flags.contains(NodeFlags::SHARED));
        let est = run.snapshot.delete_would_free(m["good"].id).unwrap();
        assert_eq!(est.at_least, 0, "{damaged_first}");
    }
}

#[test]
fn a_damaged_item_in_the_scan_that_names_nothing_readable_makes_everything_shared() {
    for damaged_first in [false, true] {
        let mut fs = MemFs::new();
        let mut bad = 0;
        if damaged_first {
            bad = fs.plain(FS, ROOT, "bad", 4096);
        }
        fs.plain(FS, ROOT, "before", 4096);
        if !damaged_first {
            bad = fs.plain(FS, ROOT, "bad", 4096);
        }
        fs.plain(FS, ROOT, "after", 8192);
        cut(&mut fs, FS, Some(bad), 20);
        let run = read(&fs.source());
        everything_is_taken_as_shared(&run, &["before", "after"]);
        assert!(run.flat()["bad"].failed);
    }
}

#[test]
fn two_items_that_disagree_about_an_extent_make_it_shared() {
    let mut fs = MemFs::new();
    let x = fs.fresh_extent(65536);
    fs.file(FS, ROOT, b"first", 65536, &[Ext::Disk(x, 65536, false)]);
    // The same address, a different size on disk.
    fs.file(FS, ROOT, b"second", 32768, &[Ext::Disk(x, 32768, false)]);
    fs.plain(FS, ROOT, "unrelated", 4096);
    let run = read(&fs.source());
    let m = run.flat();
    assert_eq!(
        run.report
            .diagnostics
            .count(ProblemKind::ExtentSizeMismatch),
        1
    );
    assert!(m["second"].failed);
    assert!(
        m["first"].flags.contains(NodeFlags::SHARED),
        "the extent was claimed by a file that was then contradicted"
    );
    assert_eq!(m["first"].exclusive, Some(0));
    let est = run.snapshot.delete_would_free(m["first"].id).unwrap();
    assert_eq!(est.at_least, 0);
    assert!(!m["unrelated"].flags.contains(NodeFlags::SHARED));
}

#[test]
fn a_source_that_serves_damaged_items_is_still_counted_once() {
    // The bytes of an extent are counted once, however it is shared.
    let (mut fs, _) = with_snapshot_copy();
    cut(&mut fs, SNAPSHOT, None, 40);
    let src: MemSource = fs.source();
    let run = read(&src);
    assert_eq!(run.snapshot.root().unwrap().allocated(), 65536 + 4096);
}
