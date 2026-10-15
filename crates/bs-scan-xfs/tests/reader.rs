//! The reader over file systems held in memory: sizes, links, the join of
//! the inode table and the directories, boundaries, and everything that can
//! go wrong in a live file system.
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

use bs_scan::{ScanContext, ScanEnd, ScanOptions};
use bs_scan_xfs::{ProblemKind, XfsError, XfsOptions, read_filesystem};
use bs_tree::{NodeFlags, NodeKind};
use common::mem::{BLOCK, DT_DIR, DT_REG, MTIME, MemXfs, ROOT};
use common::{read, try_read};

#[test]
fn files_directories_links_and_special_files_are_described() {
    let mut fs = MemXfs::new();
    let d = fs.mkdir(ROOT, "docs");
    fs.file(d, b"a.txt", 10_000, 3);
    fs.file(ROOT, b"b.bin", 4096, 1);
    fs.symlink(ROOT, "link", 10);
    fs.fifo(ROOT, "pipe");
    fs.file(ROOT, b"empty", 0, 0);
    let run = read(&fs);
    let m = run.flat();
    assert_eq!(m["docs"].kind, NodeKind::Dir);
    assert_eq!((m["docs"].logical, m["docs"].allocated), (0, 0));
    assert_eq!(m["docs"].mtime, MTIME + 1);
    assert_eq!(
        (m["docs/a.txt"].logical, m["docs/a.txt"].allocated),
        (10_000, 3 * 4096)
    );
    assert_eq!(m["docs/a.txt"].mtime, MTIME + 2);
    assert_eq!((m["b.bin"].logical, m["b.bin"].allocated), (4096, 4096));
    assert_eq!(m["link"].kind, NodeKind::Symlink);
    assert!(m["link"].flags.contains(NodeFlags::NOT_FOLLOWED));
    assert_eq!(m["pipe"].kind, NodeKind::Other);
    assert_eq!((m["empty"].logical, m["empty"].allocated), (0, 0));
    let root = run.snapshot.root().unwrap();
    assert_eq!(root.name(), b"/mnt");
    assert_eq!(root.allocated(), 4 * 4096);
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
    assert_eq!(run.report.directories, 2);
    assert_eq!(run.report.entries, 6);
    assert_eq!(run.report.looked_up, 0);
    assert_eq!(run.report.unreachable.inodes, 0);
}

#[test]
fn names_are_kept_as_the_bytes_they_are() {
    let mut fs = MemXfs::new();
    fs.file(ROOT, b"new\nline", 1, 1);
    fs.file(ROOT, b"\xff\xfe-not-utf8", 1, 1);
    let long = vec![b'n'; 255];
    fs.file(ROOT, &long, 1, 1);
    let run = read(&fs);
    let names: std::collections::BTreeSet<Vec<u8>> = run
        .snapshot
        .root()
        .unwrap()
        .children()
        .map(|c| c.name().to_vec())
        .collect();
    assert!(names.contains(&b"new\nline"[..]));
    assert!(names.contains(&b"\xff\xfe-not-utf8"[..]));
    assert!(names.contains(&long[..]));
}

#[test]
fn hard_links_count_once_and_are_flagged() {
    let mut fs = MemXfs::new();
    let d = fs.mkdir(ROOT, "d");
    let f = fs.file(ROOT, b"one", 20_000, 5);
    fs.link(d, "two", f);
    fs.link(d, "three", f);
    fs.file(ROOT, b"lonely", 20_000, 5);
    let run = read(&fs);
    let m = run.flat();
    let names = ["one", "d/two", "d/three"];
    for n in names {
        assert!(m[n].flags.contains(NodeFlags::HARD_LINK), "{n}");
    }
    let counted: u64 = names.iter().map(|n| m[*n].allocated).sum();
    assert_eq!(counted, 5 * 4096);
    let dupes = names
        .iter()
        .filter(|n| m[**n].flags.contains(NodeFlags::HARD_LINK_DUPLICATE))
        .count();
    assert_eq!(dupes, 2);
    assert!(!m["lonely"].flags.contains(NodeFlags::HARD_LINK));
    assert_eq!(run.snapshot.root().unwrap().allocated(), 10 * 4096);
    assert_eq!(run.snapshot.root().unwrap().file_count(), 4);
}

#[test]
fn a_sparse_file_is_flagged() {
    let mut fs = MemXfs::new();
    fs.file(ROOT, b"holey", 1 << 30, 3);
    fs.file(ROOT, b"dense", 12_288, 3);
    let run = read(&fs);
    let m = run.flat();
    assert!(m["holey"].flags.contains(NodeFlags::SPARSE_OR_COMPRESSED));
    assert!(!m["dense"].flags.contains(NodeFlags::SPARSE_OR_COMPRESSED));
}

#[test]
fn the_directory_walk_runs_the_same_with_any_number_of_threads() {
    let build = || {
        let mut fs = MemXfs::new();
        for i in 0..30 {
            let d = fs.mkdir(ROOT, &format!("d{i}"));
            for j in 0..8 {
                let sub = fs.mkdir(d, &format!("s{j}"));
                for k in 0..5 {
                    fs.file(sub, format!("f{k}").as_bytes(), 1000 * (k + 1), k + 1);
                }
            }
        }
        fs
    };
    let fs = build();
    let mut reference = None;
    for threads in [1usize, 2, 5, 16] {
        let options = XfsOptions {
            threads: Some(threads),
            ..XfsOptions::default()
        };
        let run = try_read(&fs, &fs, &options).unwrap();
        assert_eq!(run.applied_rejected, 0);
        let m = run.flat();
        let strip: Vec<_> = m
            .iter()
            .map(|(k, v)| (k.clone(), v.logical, v.allocated, v.mtime))
            .collect();
        assert_eq!(m.len(), 30 + 240 + 1200);
        match &reference {
            None => reference = Some(strip),
            Some(r) => assert_eq!(*r, strip, "{threads} threads"),
        }
        assert_eq!(run.report.threads, threads);
    }
}

/// Reads with the boundary allowing these extra mounts.
fn read_with_mounts(fs: &MemXfs, mounts: &[u64]) -> common::Run {
    let (ctx, rx) = ScanContext::channel(
        ScanOptions {
            boundary: bs_scan::Boundary::with_mount_ids(mounts.iter().copied()),
            ..ScanOptions::default()
        },
        4096,
    );
    let (end, report) = read_filesystem(
        fs,
        fs,
        b"/mnt",
        &XfsOptions {
            threads: Some(3),
            ..XfsOptions::default()
        },
        &ctx,
    )
    .unwrap();
    drop(ctx);
    let mut agg = bs_tree::Aggregator::new();
    for b in rx.try_iter() {
        assert_eq!(agg.apply(&b).rejected, 0);
    }
    agg.finish();
    let snapshot = agg.snapshot();
    snapshot.verify().unwrap();
    common::Run {
        snapshot,
        aggregator: agg,
        end,
        report,
        applied_rejected: 0,
        parked: 0,
        refusals: Vec::new(),
    }
}

/// A mount point is a name the listing gives with the covered directory's
/// inode, while opening it lands on the mounted root, whose inode is another.
/// That is not a replaced directory and not a corrupt parent.
#[test]
fn a_mount_point_is_not_entered_and_counts_nothing() {
    let mut fs = MemXfs::new();
    fs.file(ROOT, b"mine", 4096, 1);
    fs.mount_point(ROOT, "other", 77);
    let run = read(&fs);
    let m = run.flat();
    assert_eq!(m["other"].kind, NodeKind::Dir);
    assert!(m["other"].flags.contains(NodeFlags::NOT_FOLLOWED));
    assert!(!m.contains_key("other/inside-the-mount"));
    assert!(!m["other"].failed);
    assert_eq!(run.snapshot.root().unwrap().allocated(), 4096);
    assert!(run.snapshot.scan_complete());
    let root = run.snapshot.root().unwrap();
    assert!(root.is_complete());
    assert!(!root.is_unreadable() && !root.bytes_are_lower_bound());
    assert_eq!(
        run.report.diagnostics.total(),
        0,
        "{:?}",
        run.report.diagnostics
    );
}

#[test]
fn a_mount_of_this_file_system_that_the_caller_allows_is_entered_under_its_own_inode() {
    let mut fs = MemXfs::new();
    fs.file(ROOT, b"mine", 4096, 1);
    fs.mount_point(ROOT, "other", 77);
    let run = read_with_mounts(&fs, &[77]);
    let m = run.flat();
    assert!(m.contains_key("other/inside-the-mount"));
    assert_eq!(m["other/inside-the-mount"].allocated, 4096);
    assert!(!m["other"].flags.contains(NodeFlags::NOT_FOLLOWED));
    // The entry carries the mounted directory's numbers (its mtime and
    // block), not those of the directory it covers.
    assert_eq!(m["other"].mtime, MTIME + 10);
    assert_eq!(m["other"].allocated, 4096);
    let root = run.snapshot.root().unwrap();
    assert_eq!(root.allocated(), 4096 + 4096 + 4096);
    assert!(root.is_complete());
    assert!(!root.is_unreadable() && !root.bytes_are_lower_bound());
    assert_eq!(run.report.diagnostics.total(), 0);
    // The mounted directory's inode is accounted for, not left over.
    assert_eq!(run.report.unreachable.inodes, 0, "{:?}", run.report);
    // Nothing below the mount point is reached twice either.
    assert_eq!(
        fs.opened
            .lock()
            .unwrap()
            .iter()
            .filter(|&&i| i == ROOT)
            .count(),
        0
    );
}

#[test]
fn a_mount_of_another_file_system_is_never_entered_even_when_listed() {
    let mut fs = MemXfs::new();
    fs.file(ROOT, b"mine", 4096, 1);
    fs.foreign_mount_point(ROOT, "elsewhere", 78);
    fs.mkdir(ROOT, "plain");
    for mounts in [&[][..], &[78][..]] {
        let run = read_with_mounts(&fs, mounts);
        let m = run.flat();
        assert!(
            m["elsewhere"].flags.contains(NodeFlags::NOT_FOLLOWED),
            "{mounts:?}"
        );
        assert!(!m.contains_key("elsewhere/other-fs-file"));
        assert!(!m["elsewhere"].failed);
        assert_eq!((m["elsewhere"].logical, m["elsewhere"].allocated), (0, 0));
        assert!(m.contains_key("plain"));
        let root = run.snapshot.root().unwrap();
        assert_eq!(root.allocated(), 4096);
        assert!(root.is_complete() && !root.is_unreadable());
        assert!(!root.bytes_are_lower_bound());
        assert_eq!(
            run.report.diagnostics.total(),
            0,
            "{:?}",
            run.report.diagnostics
        );
    }
}

#[test]
fn a_mount_point_the_inode_table_does_not_hold_is_still_shown_as_not_followed() {
    let mut fs = MemXfs::new();
    fs.file(ROOT, b"mine", 4096, 1);
    let covered = fs.foreign_mount_point(ROOT, "elsewhere", 78);
    // The bulk read missed the covered directory, and the lookup through the
    // mount finds another file system's inode.
    fs.hidden.insert(covered);
    let run = read(&fs);
    let m = run.flat();
    assert!(m["elsewhere"].flags.contains(NodeFlags::NOT_FOLLOWED));
    assert!(!m["elsewhere"].failed);
    assert!(run.snapshot.root().unwrap().is_complete());
}

/// The inode check stays in force within one mount, in the root's and inside
/// an entered one: a directory that is not the one that was listed is not
/// entered, and its parent says so.
#[test]
fn a_directory_whose_inode_changed_is_replaced_within_a_mount_but_not_across_one() {
    let mut fs = MemXfs::new();
    let covered = fs.mount_point(ROOT, "other", 77);
    let root = fs.mounts[&covered];
    let listed = fs.mkdir(root, "swapped");
    fs.file(listed, b"old", 4096, 1);
    let now = fs.mkdir(root, "now");
    fs.file(now, b"new", 4096, 1);
    fs.swaps.insert(listed, now);
    let top = fs.mkdir(ROOT, "top-swapped");
    let alt = fs.mkdir(ROOT, "alt");
    fs.file(alt, b"f", 4096, 1);
    fs.swaps.insert(top, alt);
    let run = read_with_mounts(&fs, &[77]);
    let m = run.flat();
    // The mount itself is fine and entered.
    assert!(m.contains_key("other/now/new"));
    // Neither swapped directory is entered, and the directory that held each
    // is marked: its totals are a lower bound.
    assert!(!m.contains_key("other/swapped"));
    assert!(!m.contains_key("top-swapped"));
    assert_eq!(
        run.report.diagnostics.count(ProblemKind::EntryTypeMismatch),
        2
    );
    assert!(m["other"].failed);
    assert!(run.snapshot.root().unwrap().is_unreadable());
}

#[test]
fn entries_the_bulk_read_missed_are_looked_up_one_by_one() {
    let mut fs = MemXfs::new();
    fs.file(ROOT, b"old", 4096, 1);
    let new = fs.file(ROOT, b"new", 8192, 2);
    let newdir = fs.mkdir(ROOT, "newdir");
    let inside = fs.file(newdir, b"inside", 4096, 1);
    fs.hidden.extend([new, newdir, inside]);
    let run = read(&fs);
    let m = run.flat();
    assert_eq!(m["new"].allocated, 8192);
    assert_eq!(m["newdir/inside"].allocated, 4096);
    assert_eq!(run.report.looked_up, 3);
    assert_eq!(run.report.diagnostics.total(), 0);
    assert!(run.snapshot.root().unwrap().is_complete());
}

#[test]
fn an_entry_whose_inode_is_gone_is_skipped_and_counted() {
    let mut fs = MemXfs::new();
    fs.file(ROOT, b"here", 4096, 1);
    let ghost = fs.file(ROOT, b"ghost", 4096, 1);
    fs.inodes.remove(&ghost);
    let run = read(&fs);
    let m = run.flat();
    assert!(m.contains_key("here") && !m.contains_key("ghost"));
    assert_eq!(run.report.vanished_entries, 1);
    assert_eq!(
        run.report
            .diagnostics
            .count(ProblemKind::EntryToMissingInode),
        1
    );
    assert!(run.snapshot.root().unwrap().is_complete());
}

#[test]
fn an_inode_with_no_entry_is_counted_as_unreachable() {
    let mut fs = MemXfs::new();
    fs.file(ROOT, b"named", 4096, 1);
    // Removed while still open: in the table, in no directory.
    let orphan = fs.file(ROOT, b"x", 65536, 16);
    fs.dirs.get_mut(&ROOT).unwrap().retain(|e| e.ino != orphan);
    let run = read(&fs);
    assert_eq!(run.report.unreachable.inodes, 1);
    assert_eq!(run.report.unreachable.allocated, 16 * u64::from(BLOCK));
    assert_eq!(run.snapshot.root().unwrap().allocated(), 4096);
}

#[test]
fn a_listing_type_that_contradicts_the_inode_marks_the_directory() {
    let mut fs = MemXfs::new();
    let d = fs.mkdir(ROOT, "d");
    fs.file(d, b"fine", 4096, 1);
    fs.file(d, b"victim", 4096, 1);
    // The listing says directory; the inode is a file.
    fs.dirs.get_mut(&d).unwrap()[1].d_type = DT_DIR;
    let run = read(&fs);
    let m = run.flat();
    assert!(!m.contains_key("d/victim"));
    assert!(m["d"].failed);
    assert_eq!(
        run.report.diagnostics.count(ProblemKind::EntryTypeMismatch),
        1
    );
    assert!(run.snapshot.root().unwrap().bytes_are_lower_bound());
    // And the other way round: a directory the listing calls a file.
    let mut fs = MemXfs::new();
    let sub = fs.mkdir(ROOT, "sub");
    fs.dirs.get_mut(&ROOT).unwrap()[0].d_type = DT_REG;
    let _ = sub;
    let run = read(&fs);
    assert!(!run.flat().contains_key("sub"));
    assert!(run.snapshot.root().unwrap().is_unreadable());
}

#[test]
fn names_that_cannot_exist_are_dropped_and_mark_the_directory() {
    let mut fs = MemXfs::new();
    fs.file(ROOT, b"ok", 4096, 1);
    fs.file(ROOT, b"a/b", 4096, 1);
    fs.file(ROOT, b"", 4096, 1);
    let dirty = fs.file(ROOT, b"nul", 4096, 1);
    fs.dirs
        .get_mut(&ROOT)
        .unwrap()
        .iter_mut()
        .find(|e| e.ino == dirty)
        .unwrap()
        .clean = false;
    let run = read(&fs);
    let m = run.flat();
    assert_eq!(m.len(), 1);
    assert_eq!(run.report.diagnostics.count(ProblemKind::EntryBadName), 3);
    assert!(run.snapshot.root().unwrap().is_unreadable());
}

#[test]
fn a_directory_entered_twice_is_entered_once() {
    let mut fs = MemXfs::new();
    let a = fs.mkdir(ROOT, "a");
    fs.file(a, b"inside", 4096, 1);
    fs.link_dir(ROOT, "again", a);
    let run = read(&fs);
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
    let mut fs = MemXfs::new();
    let a = fs.mkdir(ROOT, "a");
    let b = fs.mkdir(a, "b");
    fs.link_dir(b, "up", a);
    fs.link_dir(a, "root", ROOT);
    let run = read(&fs);
    assert_eq!(run.flat().len(), 2);
    assert!(
        run.report
            .diagnostics
            .count(ProblemKind::DirectoryLinkedTwice)
            >= 1
    );
}

#[test]
fn a_directory_that_cannot_be_opened_is_unreadable_and_the_rest_is_kept() {
    let mut fs = MemXfs::new();
    fs.file(ROOT, b"fine", 4096, 1);
    let locked = fs.mkdir(ROOT, "locked");
    fs.file(locked, b"secret", 4096, 1);
    fs.open_errors.insert(locked, 13);
    let run = read(&fs);
    let m = run.flat();
    assert!(m["locked"].failed);
    assert!(!m.contains_key("locked/secret"));
    assert_eq!(m["fine"].allocated, 4096);
    assert_eq!(
        run.report
            .diagnostics
            .count(ProblemKind::DirectoryUnreadable),
        1
    );
    let root = run.snapshot.root().unwrap();
    assert!(root.has_unreadable_below());
    let failure = run
        .snapshot
        .node(m["locked"].id)
        .unwrap()
        .read_failure()
        .unwrap();
    assert_eq!(failure.kind, bs_tree::ReadErrorKind::PermissionDenied);
}

#[test]
fn a_directory_replaced_or_removed_since_it_was_listed_is_dealt_with_as_what_it_is_now() {
    // Replaced by a file with the same inode number (the table is stale): the
    // entry contradicts the table, so it is dropped and its directory marked.
    let mut fs = MemXfs::new();
    let swapped = fs.mkdir(ROOT, "was-a-dir");
    fs.file(ROOT, b"fine", 4096, 1);
    fs.open_errors.insert(swapped, 20);
    let run = read(&fs);
    let m = run.flat();
    assert!(!m.contains_key("was-a-dir") && m.contains_key("fine"));
    assert_eq!(
        run.report.diagnostics.count(ProblemKind::EntryTypeMismatch),
        1
    );
    assert!(run.snapshot.root().unwrap().is_unreadable());

    // Removed: counted, and nothing is marked, as a file removed a moment
    // ago is.
    let mut fs = MemXfs::new();
    let gone = fs.mkdir(ROOT, "removed");
    fs.file(ROOT, b"fine", 4096, 1);
    fs.open_errors.insert(gone, 2);
    let run = read(&fs);
    assert!(!run.flat().contains_key("removed"));
    assert_eq!(run.report.vanished_entries, 1);
    assert!(run.snapshot.root().unwrap().is_complete());
    assert!(!run.snapshot.root().unwrap().is_unreadable());
}

#[test]
fn a_listing_that_fails_partway_keeps_what_it_gave_and_marks_the_directory() {
    let mut fs = MemXfs::new();
    let d = fs.mkdir(ROOT, "d");
    for i in 0..10 {
        fs.file(d, format!("f{i}").as_bytes(), 4096, 1);
    }
    fs.list_errors.insert(d, 4);
    let run = read(&fs);
    let m = run.flat();
    assert!(m["d"].failed);
    assert_eq!(m.keys().filter(|k| k.starts_with("d/")).count(), 4);
    assert_eq!(run.report.diagnostics.count(ProblemKind::ListingCorrupt), 1);
}

#[test]
fn an_inode_with_an_impossible_mode_is_shown_unreadable() {
    let mut fs = MemXfs::new();
    let f = fs.file(ROOT, b"weird", 4096, 1);
    fs.inodes.get_mut(&f).unwrap().mode = 0o030_000;
    let run = read(&fs);
    let m = run.flat();
    assert!(m["weird"].failed);
    assert_eq!(m["weird"].kind, NodeKind::Other);
    assert_eq!(run.report.diagnostics.count(ProblemKind::InodeInvalid), 1);
    assert!(run.snapshot.root().unwrap().has_unreadable_below());
}

#[test]
fn a_bulk_read_that_fails_is_an_error_and_not_a_tree() {
    let mut fs = MemXfs::new();
    for i in 0..10 {
        fs.file(ROOT, format!("f{i}").as_bytes(), 1, 1);
    }
    fs.bulk_fails_after = Some(3);
    let error = try_read(&fs, &fs, &XfsOptions::default())
        .err()
        .expect("an error");
    assert!(matches!(error, XfsError::Io(_)), "{error}");
}

#[test]
fn the_memory_limit_refuses_what_it_cannot_hold() {
    let mut fs = MemXfs::new();
    for i in 0..2000 {
        fs.file(ROOT, format!("file-{i}").as_bytes(), 4096, 1);
    }
    for limit in [0, 1000, 10_000, 100_000] {
        let options = XfsOptions {
            max_memory_bytes: limit,
            ..XfsOptions::default()
        };
        let error = try_read(&fs, &fs, &options).err().expect("refused");
        assert!(matches!(error, XfsError::TooLarge(_)), "{limit}: {error}");
    }
    let options = XfsOptions {
        max_memory_bytes: 64 << 20,
        ..XfsOptions::default()
    };
    let run = try_read(&fs, &fs, &options).unwrap();
    assert!(run.report.memory_used_bytes > 0);
    assert!(run.report.memory_used_bytes < 64 << 20);
}

#[test]
fn a_very_deep_tree_is_cut_off_and_marked() {
    let mut fs = MemXfs::new();
    let mut at = ROOT;
    for _ in 0..66_000 {
        at = fs.mkdir(at, "d");
    }
    fs.file(at, b"bottom", 4096, 1);
    let run = read(&fs);
    assert!(run.report.diagnostics.count(ProblemKind::TooDeep) >= 1);
    assert!(run.snapshot.root().unwrap().bytes_are_lower_bound());
}

#[test]
fn cancelling_keeps_the_partial_tree_marked_incomplete() {
    let mut fs = MemXfs::new();
    for i in 0..50 {
        fs.file(ROOT, format!("f{i}").as_bytes(), 4096, 1);
    }
    let (ctx, rx) = ScanContext::channel(ScanOptions::default(), 4096);
    ctx.cancel();
    let (end, report) = read_filesystem(&fs, &fs, b"/mnt", &XfsOptions::default(), &ctx).unwrap();
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
    let mut fs = MemXfs::new();
    fs.file(ROOT, b"clip.mp4", 4096, 1);
    fs.file(ROOT, b"README", 4096, 1);
    let run = read(&fs);
    let m = run.flat();
    assert_ne!(m["clip.mp4"].category, 0);
    assert_eq!(m["README"].category, 0);
}

#[test]
fn reading_twice_gives_the_same_tree() {
    let mut fs = MemXfs::new();
    for i in 0..30 {
        let d = fs.mkdir(ROOT, &format!("d{i}"));
        for j in 0..10 {
            fs.file(d, format!("f{j}").as_bytes(), 1000 * (j + 1), j + 1);
        }
    }
    let strip = |m: &std::collections::BTreeMap<String, common::Info>| {
        m.iter()
            .map(|(k, v)| (k.clone(), v.logical, v.allocated, v.mtime, v.flags))
            .collect::<Vec<_>>()
    };
    assert_eq!(strip(&read(&fs).flat()), strip(&read(&fs).flat()));
}
