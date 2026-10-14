//! Behavior of the aggregator: sizes, completeness, unreadable entries, hard
//! links, out-of-order arrival, and refusal of bad input.

mod common;

use bs_tree::{
    Aggregator, ApplierLimits, ApplyError, Batch, DirKey, FileKey, Meta, NodeFlags, NodeId,
    NodeKind, Progress, ReadErrorKind, ReadFailure,
};
use common::*;

fn sized(logical: u64, allocated: u64) -> Meta {
    Meta {
        logical,
        allocated,
        ..Meta::default()
    }
}

fn root_batch(batch: &mut Batch) {
    batch
        .dir(DirKey(0), DirKey::NONE, b"/r", &sized(0, 4096))
        .unwrap();
}

fn id_by_path(snap: &bs_tree::Snapshot, path: &[u8]) -> NodeId {
    let mut found = None;
    for i in 0..snap.len() {
        if snap.path(NodeId::from_index(i), b'/').as_deref() == Some(path) {
            found = Some(NodeId::from_index(i));
        }
    }
    found.unwrap_or_else(|| panic!("no node at {}", String::from_utf8_lossy(path)))
}

#[test]
fn sizes_and_file_counts_propagate_to_every_ancestor() {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    root_batch(&mut b);
    b.dir(DirKey(1), DirKey(0), b"a", &sized(0, 4096)).unwrap();
    b.dir(DirKey(2), DirKey(1), b"b", &sized(0, 4096)).unwrap();
    b.file(NodeKind::File, DirKey(2), b"f1", &sized(100, 4096))
        .unwrap();
    b.file(NodeKind::File, DirKey(2), b"f2", &sized(5000, 8192))
        .unwrap();
    b.file(NodeKind::Symlink, DirKey(0), b"l", &sized(7, 0))
        .unwrap();
    let report = agg.apply(&b);
    assert_eq!(report.rejected, 0);
    assert_eq!(report.applied, 6);
    let snap = agg.publish();
    snap.verify().unwrap();

    let root = snap.root().unwrap();
    assert_eq!(root.logical(), 100 + 5000 + 7);
    assert_eq!(root.allocated(), 4096 * 3 + 4096 + 8192);
    assert_eq!(root.file_count(), 3);
    let b_node = snap.node(id_by_path(&snap, b"/r/a/b")).unwrap();
    assert_eq!(b_node.allocated(), 4096 + 4096 + 8192);
    assert_eq!(b_node.file_count(), 2);
    assert_eq!(b_node.kind(), NodeKind::Dir);
    assert_eq!(b_node.parent().unwrap().name(), b"a");
}

#[test]
fn directory_with_zero_children_completes_on_listing_done() {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    root_batch(&mut b);
    b.listing_done(DirKey(0));
    agg.apply(&b);
    let snap = agg.publish();
    let root = snap.root().unwrap();
    assert!(root.is_complete());
    assert!(!root.is_in_progress());
    assert!(!root.bytes_are_lower_bound());
    assert!(snap.scan_complete());
    assert_eq!(root.children().count(), 0);
    assert_eq!(snap.progress(), Progress::Ratio(1.0));
}

#[test]
fn completion_waits_for_listing_and_every_subdirectory_then_cascades() {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    root_batch(&mut b);
    b.dir(DirKey(1), DirKey(0), b"a", &Meta::default()).unwrap();
    b.dir(DirKey(2), DirKey(1), b"b", &Meta::default()).unwrap();
    b.dir(DirKey(3), DirKey(1), b"c", &Meta::default()).unwrap();
    b.listing_done(DirKey(0));
    b.listing_done(DirKey(1));
    agg.apply(&b);
    let snap = agg.publish();
    // Listings of the root and "a" are done but b and c are not.
    for path in [&b"/r"[..], b"/r/a", b"/r/a/b", b"/r/a/c"] {
        let n = snap.node(id_by_path(&snap, path)).unwrap();
        assert!(n.is_in_progress(), "{}", String::from_utf8_lossy(path));
        assert!(n.bytes_are_lower_bound());
    }
    assert!(!snap.scan_complete());

    let mut b = Batch::new();
    b.listing_done(DirKey(2));
    agg.apply(&b);
    let snap = agg.publish();
    assert!(
        snap.node(id_by_path(&snap, b"/r/a/b"))
            .unwrap()
            .is_complete()
    );
    assert!(
        snap.node(id_by_path(&snap, b"/r/a"))
            .unwrap()
            .is_in_progress()
    );

    let mut b = Batch::new();
    b.listing_done(DirKey(3));
    agg.apply(&b);
    let snap = agg.publish();
    // The last pending directory completes everything above it at once.
    for path in [&b"/r"[..], b"/r/a", b"/r/a/b", b"/r/a/c"] {
        assert!(snap.node(id_by_path(&snap, path)).unwrap().is_complete());
    }
    assert!(snap.scan_complete());
    snap.verify().unwrap();
}

#[test]
fn children_arriving_before_their_parent_wait_for_it() {
    // Same tree applied child-first and parent-first must give the same result.
    let spec = spec_from_raw(&[
        (0, true, 0, 8),
        (1, true, 0, 8),
        (1, false, 10, 8),
        (2, false, 20, 8),
        (0, false, 30, 8),
    ]);
    let batches = batches_for(&spec);
    let mut forward = Aggregator::new();
    for b in &batches {
        assert_eq!(forward.apply(b).rejected, 0);
    }
    let mut backward = Aggregator::new();
    let mut parked = 0;
    for b in batches.iter().rev() {
        let r = backward.apply(b);
        assert_eq!(r.rejected, 0);
        parked += r.parked;
    }
    assert!(parked > 0, "reverse order must exercise the waiting path");
    assert_eq!(backward.finish().unresolved, 0);
    let (f, r) = (forward.publish(), backward.publish());
    assert!(f.scan_complete() && r.scan_complete());
    r.verify().unwrap();
    let expected = expected_totals(&spec);
    for (i, want) in expected.iter().enumerate() {
        let path = if i == 0 {
            b"/root".to_vec()
        } else {
            let mut parts = vec![];
            let mut at = i;
            while at != 0 {
                parts.push(name_of(at));
                at = spec.nodes[at].parent;
            }
            let mut p = b"/root".to_vec();
            for part in parts.iter().rev() {
                p.push(b'/');
                p.extend_from_slice(part);
            }
            p
        };
        for snap in [&f, &r] {
            let n = snap.node(id_by_path(snap, &path)).unwrap();
            assert_eq!(
                (n.logical(), n.allocated(), u64::from(n.file_count())),
                *want
            );
            assert!(n.is_complete());
        }
    }
}

#[test]
fn listing_done_before_the_directory_entry_is_held_too() {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.listing_done(DirKey(1));
    let r = agg.apply(&b);
    assert_eq!((r.parked, r.rejected), (1, 0));
    let mut b = Batch::new();
    root_batch(&mut b);
    b.dir(DirKey(1), DirKey(0), b"a", &Meta::default()).unwrap();
    b.listing_done(DirKey(0));
    agg.apply(&b);
    let snap = agg.publish();
    assert!(snap.scan_complete());
}

#[test]
fn unreadable_directory_is_kept_flagged_and_counts_toward_completeness() {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    root_batch(&mut b);
    b.dir(DirKey(1), DirKey(0), b"locked", &sized(0, 4096))
        .unwrap();
    b.dir(DirKey(2), DirKey(1), b"sub", &Meta::default())
        .unwrap();
    b.listing_done(DirKey(2));
    b.unreadable(
        DirKey(1),
        ReadFailure {
            kind: ReadErrorKind::PermissionDenied,
            code: 13,
        },
    );
    b.file(NodeKind::File, DirKey(0), b"ok", &sized(1, 4096))
        .unwrap();
    b.listing_done(DirKey(0));
    agg.apply(&b);
    let snap = agg.publish();
    snap.verify().unwrap();

    let locked = snap.node(id_by_path(&snap, b"/r/locked")).unwrap();
    assert!(locked.is_unreadable());
    assert!(locked.is_complete(), "unreadable counts as finished");
    assert_eq!(
        locked.read_failure(),
        Some(ReadFailure {
            kind: ReadErrorKind::PermissionDenied,
            code: 13
        })
    );
    assert!(locked.bytes_are_lower_bound());
    let root = snap.root().unwrap();
    assert!(root.is_complete());
    assert!(root.has_unreadable_below());
    assert!(
        root.bytes_are_lower_bound(),
        "complete but still a lower bound"
    );
    assert!(!root.is_unreadable());
    assert!(snap.scan_complete());
    // A sibling that was readable is exact.
    assert!(
        !snap
            .node(id_by_path(&snap, b"/r/ok"))
            .unwrap()
            .bytes_are_lower_bound()
    );
}

#[test]
fn unreadable_file_marks_its_ancestors() {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    root_batch(&mut b);
    b.dir(DirKey(1), DirKey(0), b"d", &Meta::default()).unwrap();
    let failing = Meta {
        failure: Some(ReadFailure {
            kind: ReadErrorKind::Io,
            code: 5,
        }),
        ..Meta::default()
    };
    b.file(NodeKind::File, DirKey(1), b"bad", &failing).unwrap();
    b.listing_done(DirKey(1));
    b.listing_done(DirKey(0));
    agg.apply(&b);
    let snap = agg.publish();
    snap.verify().unwrap();
    let bad = snap.node(id_by_path(&snap, b"/r/d/bad")).unwrap();
    assert!(bad.is_unreadable());
    assert_eq!(bad.read_failure().unwrap().kind, ReadErrorKind::Io);
    assert!(
        snap.node(id_by_path(&snap, b"/r/d"))
            .unwrap()
            .has_unreadable_below()
    );
    assert!(snap.root().unwrap().has_unreadable_below());
}

#[test]
fn unreadable_directory_may_still_report_partial_contents() {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    root_batch(&mut b);
    b.dir(DirKey(1), DirKey(0), b"half", &Meta::default())
        .unwrap();
    b.file(NodeKind::File, DirKey(1), b"seen", &sized(5, 4096))
        .unwrap();
    b.unreadable(
        DirKey(1),
        ReadFailure {
            kind: ReadErrorKind::Io,
            code: 5,
        },
    );
    // A listing-done after the failure is harmless.
    b.listing_done(DirKey(1));
    b.listing_done(DirKey(0));
    let r = agg.apply(&b);
    assert_eq!(r.rejected, 0);
    let snap = agg.publish();
    assert_eq!(
        snap.node(id_by_path(&snap, b"/r/half"))
            .unwrap()
            .file_count(),
        1
    );
    assert!(snap.scan_complete());
}

fn link(volume: u64, id: u128) -> Option<FileKey> {
    Some(FileKey { volume, id })
}

#[test]
fn hard_links_count_once_on_first_sighting_and_are_flagged() {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    root_batch(&mut b);
    b.dir(DirKey(1), DirKey(0), b"d1", &Meta::default())
        .unwrap();
    b.dir(DirKey(2), DirKey(0), b"d2", &Meta::default())
        .unwrap();
    let linked = Meta {
        link: link(1, 42),
        ..sized(1000, 4096)
    };
    b.file(NodeKind::File, DirKey(1), b"first", &linked)
        .unwrap();
    // Same file seen again in another directory, and in a later batch.
    b.file(NodeKind::File, DirKey(2), b"second", &linked)
        .unwrap();
    agg.apply(&b);
    let mut b = Batch::new();
    b.file(NodeKind::File, DirKey(2), b"third", &linked)
        .unwrap();
    // Same inode number on another volume is a different file.
    let other_volume = Meta {
        link: link(2, 42),
        ..sized(1000, 4096)
    };
    b.file(NodeKind::File, DirKey(2), b"other-volume", &other_volume)
        .unwrap();
    // No identity: never a hard link, even with identical sizes.
    b.file(NodeKind::File, DirKey(2), b"plain-a", &sized(1000, 4096))
        .unwrap();
    b.file(NodeKind::File, DirKey(2), b"plain-b", &sized(1000, 4096))
        .unwrap();
    b.listing_done(DirKey(1));
    b.listing_done(DirKey(2));
    b.listing_done(DirKey(0));
    agg.apply(&b);
    let snap = agg.publish();
    snap.verify().unwrap();

    let first = snap.node(id_by_path(&snap, b"/r/d1/first")).unwrap();
    assert!(first.is_hard_link());
    assert_eq!(first.allocated(), 4096);
    assert_eq!(first.hard_link_origin(), None);
    for name in [&b"/r/d2/second"[..], b"/r/d2/third"] {
        let dup = snap.node(id_by_path(&snap, name)).unwrap();
        assert!(dup.is_hard_link());
        assert!(dup.flags().contains(NodeFlags::HARD_LINK_DUPLICATE));
        assert_eq!((dup.logical(), dup.allocated()), (0, 0));
        assert_eq!(dup.file_count(), 1, "the name is still an entry");
        assert_eq!(dup.hard_link_origin(), Some(first.id()));
    }
    let other = snap.node(id_by_path(&snap, b"/r/d2/other-volume")).unwrap();
    assert!(
        !other.is_hard_link(),
        "same inode on another volume is a different file, seen once"
    );
    assert_eq!(other.allocated(), 4096);
    for name in [&b"/r/d2/plain-a"[..], b"/r/d2/plain-b"] {
        let plain = snap.node(id_by_path(&snap, name)).unwrap();
        assert!(!plain.is_hard_link());
        assert_eq!(plain.allocated(), 4096);
    }
    // Root: own 4096 + first + other-volume + two plain files.
    assert_eq!(snap.root().unwrap().allocated(), 4096 + 4 * 4096);
    assert_eq!(snap.root().unwrap().logical(), 4000);
}

#[test]
fn link_key_on_a_directory_is_ignored() {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    root_batch(&mut b);
    let m = Meta {
        link: link(1, 1),
        ..sized(0, 4096)
    };
    b.dir(DirKey(1), DirKey(0), b"d1", &m).unwrap();
    b.dir(DirKey(2), DirKey(0), b"d2", &m).unwrap();
    let r = agg.apply(&b);
    assert_eq!(r.rejected, 0);
    let snap = agg.publish();
    assert_eq!(snap.root().unwrap().allocated(), 3 * 4096);
}

#[test]
fn names_are_raw_bytes_and_overlong_names_are_flagged() {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    root_batch(&mut b);
    let odd: &[u8] = b"bad\xff\xfe name\nwith newline";
    b.file(NodeKind::File, DirKey(0), odd, &Meta::default())
        .unwrap();
    b.file(NodeKind::File, DirKey(0), b"", &Meta::default())
        .unwrap();
    let long = vec![b'x'; 5000];
    b.file(NodeKind::File, DirKey(0), &long, &Meta::default())
        .unwrap();
    b.listing_done(DirKey(0));
    agg.apply(&b);
    let snap = agg.publish();
    let root = snap.root().unwrap();
    let mut names: Vec<(usize, bool)> = root
        .children()
        .map(|c| {
            (
                c.name().len(),
                c.flags().contains(NodeFlags::NAME_TRUNCATED),
            )
        })
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            (0, false),
            (odd.len(), false),
            (bs_tree::MAX_NAME_LEN, true)
        ]
    );
    assert!(root.children().any(|c| c.name() == odd));
    assert_eq!(snap.path(NodeId::ROOT, b'/').unwrap(), b"/r");
}

#[test]
fn path_joins_without_doubling_separators() {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"C:\\", &Meta::default())
        .unwrap();
    b.dir(DirKey(1), DirKey(0), b"Users", &Meta::default())
        .unwrap();
    b.file(NodeKind::File, DirKey(1), b"x.txt", &Meta::default())
        .unwrap();
    agg.apply(&b);
    let snap = agg.publish();
    let file = snap.node(NodeId::from_index(2)).unwrap();
    assert_eq!(snap.path(file.id(), b'\\').unwrap(), b"C:\\Users\\x.txt");
    assert_eq!(snap.path(NodeId::from_index(99), b'\\'), None);
}

#[test]
fn children_by_size_sorts_largest_first_with_stable_ties() {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    root_batch(&mut b);
    for (i, size) in [10u64, 500, 10, 500, 70].into_iter().enumerate() {
        b.file(
            NodeKind::File,
            DirKey(0),
            format!("f{i}").as_bytes(),
            &sized(size, size),
        )
        .unwrap();
    }
    agg.apply(&b);
    let snap = agg.publish();
    let order: Vec<Vec<u8>> = snap
        .children_by_size(NodeId::ROOT)
        .into_iter()
        .map(|id| snap.node(id).unwrap().name().to_vec())
        .collect();
    let want: Vec<Vec<u8>> = ["f1", "f3", "f4", "f0", "f2"]
        .iter()
        .map(|s| s.as_bytes().to_vec())
        .collect();
    assert_eq!(order, want);
    assert!(snap.children_by_size(NodeId::from_index(1)).is_empty());
    assert!(snap.children_by_size(NodeId::from_index(77)).is_empty());
}

#[test]
fn progress_uses_ratio_until_counted_bytes_exceed_used() {
    let mut agg = Aggregator::new();
    assert_eq!(agg.publish().progress(), Progress::Files(0));
    let mut b = Batch::new();
    root_batch(&mut b);
    b.file(NodeKind::File, DirKey(0), b"f", &sized(1, 4096))
        .unwrap();
    agg.apply(&b);
    assert_eq!(
        agg.publish().progress(),
        Progress::Files(1),
        "no OS figure yet"
    );
    agg.set_os_used_bytes(Some(16384));
    let snap = agg.publish();
    assert_eq!(snap.progress(), Progress::Ratio(0.5));
    assert!(!snap.counted_exceeds_used());
    agg.set_os_used_bytes(Some(4096));
    let snap = agg.publish();
    assert!(snap.counted_exceeds_used());
    assert_eq!(snap.progress(), Progress::Files(1));
    assert_eq!(snap.os_used_bytes(), Some(4096));
}

#[test]
fn stopped_scan_keeps_partial_results_marked_incomplete() {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    root_batch(&mut b);
    b.dir(DirKey(1), DirKey(0), b"a", &Meta::default()).unwrap();
    b.file(NodeKind::File, DirKey(1), b"f", &sized(9, 4096))
        .unwrap();
    agg.apply(&b);
    agg.mark_stopped();
    let snap = agg.publish();
    assert!(snap.scan_stopped());
    assert!(!snap.scan_complete());
    assert!(snap.root().unwrap().is_in_progress());
    assert_eq!(snap.root().unwrap().allocated(), 4096 + 4096);
}

// ---- refusals -------------------------------------------------------------

fn first_error(r: &bs_tree::ApplyReport) -> ApplyError {
    r.first_errors.first().expect("an error").1
}

#[test]
fn bad_input_is_refused_not_applied_and_does_not_stop_the_batch() {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    root_batch(&mut b);
    b.dir(DirKey(1), DirKey(0), b"a", &Meta::default()).unwrap();
    agg.apply(&b);

    let cases: Vec<(Batch, ApplyError)> = {
        let mut v = Vec::new();
        let mut x = Batch::new();
        x.dir(DirKey(1), DirKey(0), b"dup", &Meta::default())
            .unwrap();
        v.push((x, ApplyError::DuplicateKey(DirKey(1))));
        let mut x = Batch::new();
        x.dir(DirKey(5), DirKey(5), b"self", &Meta::default())
            .unwrap();
        v.push((x, ApplyError::SelfParent));
        let mut x = Batch::new();
        x.dir(DirKey::NONE, DirKey(0), b"reserved", &Meta::default())
            .unwrap();
        v.push((x, ApplyError::ReservedKey));
        let mut x = Batch::new();
        x.dir(DirKey(6), DirKey::NONE, b"second-root", &Meta::default())
            .unwrap();
        v.push((x, ApplyError::RootAlreadyPresent));
        let mut x = Batch::new();
        x.file(
            NodeKind::File,
            DirKey::NONE,
            b"second-root",
            &Meta::default(),
        )
        .unwrap();
        v.push((x, ApplyError::RootAlreadyPresent));
        v
    };
    for (batch, want) in cases {
        let r = agg.apply(&batch);
        assert_eq!((r.applied, r.rejected), (0, 1), "{want}");
        assert_eq!(first_error(&r), want);
    }
    let snap = agg.publish();
    assert_eq!(snap.len(), 2, "nothing was added by refused records");
    assert_eq!(snap.lost_records(), 5);
    assert!(!snap.scan_complete());
    snap.verify().unwrap();

    // One bad record among good ones: the good ones land.
    let mut b = Batch::new();
    b.file(NodeKind::File, DirKey(1), b"good1", &sized(1, 1))
        .unwrap();
    b.dir(DirKey(1), DirKey(0), b"dup", &Meta::default())
        .unwrap();
    b.file(NodeKind::File, DirKey(1), b"good2", &sized(1, 1))
        .unwrap();
    let r = agg.apply(&b);
    assert_eq!((r.applied, r.rejected), (2, 1));
    assert_eq!(first_error(&r), ApplyError::DuplicateKey(DirKey(1)));
    assert_eq!(r.first_errors[0].0, Some(1), "position in the batch");
}

#[test]
fn first_record_must_be_a_directory() {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.file(NodeKind::File, DirKey::NONE, b"f", &Meta::default())
        .unwrap();
    let r = agg.apply(&b);
    assert_eq!(first_error(&r), ApplyError::RootMustBeDirectory);
    assert!(agg.publish().is_empty());
}

#[test]
fn nothing_may_be_added_to_a_complete_directory() {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    root_batch(&mut b);
    b.dir(DirKey(1), DirKey(0), b"a", &Meta::default()).unwrap();
    b.listing_done(DirKey(1));
    agg.apply(&b);

    let mut b = Batch::new();
    b.file(NodeKind::File, DirKey(1), b"late", &sized(9, 9))
        .unwrap();
    b.dir(DirKey(2), DirKey(1), b"late-dir", &Meta::default())
        .unwrap();
    b.listing_done(DirKey(1));
    let r = agg.apply(&b);
    assert_eq!(r.rejected, 3);
    assert_eq!(r.first_errors[0].1, ApplyError::ParentComplete);
    assert_eq!(r.first_errors[2].1, ApplyError::ListingAlreadyDone);
    let snap = agg.publish();
    assert!(snap.node(id_by_path(&snap, b"/r/a")).unwrap().is_complete());
    snap.verify().unwrap();
}

#[test]
fn records_for_a_missing_parent_are_lost_not_silently_dropped() {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    root_batch(&mut b);
    b.file(NodeKind::File, DirKey(77), b"orphan", &sized(1, 1))
        .unwrap();
    b.listing_done(DirKey(0));
    let r = agg.apply(&b);
    assert_eq!((r.parked, r.rejected), (1, 0));
    assert_eq!(agg.publish().lost_records(), 1);
    assert!(
        !agg.publish().scan_complete(),
        "waiting records block 'complete'"
    );
    assert_eq!(agg.finish().unresolved, 1);
    let snap = agg.publish();
    assert_eq!(snap.lost_records(), 1);
    assert!(!snap.scan_complete());
    let root = snap.root().unwrap();
    assert!(root.is_complete(), "structurally complete, but not exact");
    assert!(root.has_lost_records_below());
    assert!(
        root.bytes_are_lower_bound(),
        "lost records make the root a lower bound"
    );
    snap.verify().unwrap();
}

#[test]
fn parent_cycles_are_detected() {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    root_batch(&mut b);
    b.dir(DirKey(10), DirKey(11), b"x", &Meta::default())
        .unwrap();
    b.dir(DirKey(11), DirKey(12), b"y", &Meta::default())
        .unwrap();
    b.dir(DirKey(12), DirKey(10), b"z", &Meta::default())
        .unwrap();
    let r = agg.apply(&b);
    assert_eq!(r.parked, 2);
    assert_eq!(r.rejected, 1);
    assert_eq!(first_error(&r), ApplyError::Cycle);
    let snap = agg.publish();
    assert_eq!(snap.len(), 1);
    snap.verify().unwrap();
}

#[test]
fn waiting_records_are_capped() {
    let mut agg = Aggregator::with_limits(ApplierLimits {
        max_parked_records: 3,
        ..ApplierLimits::default()
    });
    let mut b = Batch::new();
    for i in 0..5 {
        b.file(
            NodeKind::File,
            DirKey(9),
            format!("f{i}").as_bytes(),
            &Meta::default(),
        )
        .unwrap();
    }
    let r = agg.apply(&b);
    assert_eq!((r.parked, r.rejected), (3, 2));
    assert_eq!(first_error(&r), ApplyError::ParkLimit);

    let mut agg = Aggregator::with_limits(ApplierLimits {
        max_parked_name_bytes: 10,
        ..ApplierLimits::default()
    });
    let mut b = Batch::new();
    b.file(NodeKind::File, DirKey(9), &[b'a'; 11], &Meta::default())
        .unwrap();
    assert_eq!(first_error(&agg.apply(&b)), ApplyError::ParkLimit);
}

#[test]
fn node_id_exhaustion_is_an_error_not_a_wrap() {
    let mut agg = Aggregator::with_limits(ApplierLimits {
        max_nodes: 3,
        ..ApplierLimits::default()
    });
    let mut b = Batch::new();
    root_batch(&mut b);
    for i in 0..4 {
        b.file(
            NodeKind::File,
            DirKey(0),
            format!("f{i}").as_bytes(),
            &sized(1, 1),
        )
        .unwrap();
    }
    let r = agg.apply(&b);
    assert_eq!(r.applied, 3);
    assert_eq!(r.rejected, 2);
    assert_eq!(first_error(&r), ApplyError::NodeIdsExhausted);
    let snap = agg.publish();
    assert_eq!(snap.len(), 3);
    snap.verify().unwrap();
    assert_eq!(snap.root().unwrap().allocated(), 4096 + 2);
}

#[test]
fn size_overflow_is_refused() {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    root_batch(&mut b);
    // The root's own entry is 4096, so this brings the total to MAX - 10.
    b.file(
        NodeKind::File,
        DirKey(0),
        b"huge",
        &sized(5, u64::MAX - 4106),
    )
    .unwrap();
    b.file(NodeKind::File, DirKey(0), b"more", &sized(5, 11))
        .unwrap();
    b.file(NodeKind::File, DirKey(0), b"fits", &sized(5, 10))
        .unwrap();
    let r = agg.apply(&b);
    assert_eq!((r.applied, r.rejected), (3, 1));
    assert_eq!(first_error(&r), ApplyError::SizeOverflow);
    let snap = agg.publish();
    snap.verify().unwrap();
    assert_eq!(snap.root().unwrap().allocated(), u64::MAX);
}

#[test]
fn spec_trees_match_the_oracle_in_any_batch_order() {
    let raw: Vec<(u32, bool, u16, u16)> = (0..200u32)
        .map(|i| {
            (
                i.wrapping_mul(2654435761) >> 7,
                i % 4 == 0,
                (i * 37 % 1000) as u16,
                (i % 9) as u16,
            )
        })
        .collect();
    let spec = spec_from_raw(&raw);
    let expected = expected_totals(&spec);
    for seed in 1..=20u64 {
        let mut batches = batches_for(&spec);
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15)).shuffle(&mut batches);
        let mut agg = Aggregator::new();
        for b in &batches {
            assert_eq!(agg.apply(b).rejected, 0);
        }
        assert_eq!(agg.finish().unresolved, 0);
        let snap = agg.publish();
        snap.verify().unwrap();
        assert!(snap.scan_complete(), "seed {seed}");
        assert_eq!(snap.len() as usize, spec.nodes.len());
        let root = snap.root().unwrap();
        assert_eq!(
            (
                root.logical(),
                root.allocated(),
                u64::from(root.file_count())
            ),
            expected[0]
        );
        // Every named node matches its oracle total.
        for i in 0..snap.len() {
            let n = snap.node(NodeId::from_index(i)).unwrap();
            let name = n.name();
            let idx: usize = if name == b"/root" {
                0
            } else {
                std::str::from_utf8(&name[1..]).unwrap().parse().unwrap()
            };
            assert_eq!(
                (n.logical(), n.allocated(), u64::from(n.file_count())),
                expected[idx]
            );
            assert!(n.is_complete());
        }
    }
}

#[test]
fn a_keyed_file_seen_once_is_not_a_hard_link() {
    // A scanner on a volume with no link counts keys every file. Nothing is
    // a hard link until a second name shows up.
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    root_batch(&mut b);
    for i in 0..5u128 {
        let m = Meta {
            link: link(1, i),
            ..sized(10, 4096)
        };
        b.file(NodeKind::File, DirKey(0), format!("f{i}").as_bytes(), &m)
            .unwrap();
    }
    b.listing_done(DirKey(0));
    agg.apply(&b);
    let snap = agg.publish();
    for child in snap.root().unwrap().children() {
        assert!(!child.is_hard_link(), "{:?}", child.flags());
        assert!(!child.flags().contains(NodeFlags::HARD_LINK_DUPLICATE));
        assert_eq!(child.hard_link_origin(), None);
        assert_eq!(child.allocated(), 4096);
    }

    // The second name of one of them flags both names, and only those.
    let mut b = Batch::new();
    let mut agg = Aggregator::new();
    root_batch(&mut b);
    b.dir(DirKey(1), DirKey(0), b"other", &Meta::default())
        .unwrap();
    let m = Meta {
        link: link(1, 3),
        ..sized(10, 4096)
    };
    b.file(NodeKind::File, DirKey(0), b"a", &m).unwrap();
    let lone = Meta {
        link: link(1, 4),
        ..sized(10, 4096)
    };
    b.file(NodeKind::File, DirKey(0), b"lone", &lone).unwrap();
    agg.apply(&b);
    let before = agg.publish();
    assert!(
        !before
            .node(id_by_path(&before, b"/r/a"))
            .unwrap()
            .is_hard_link()
    );
    let mut b = Batch::new();
    b.file(NodeKind::File, DirKey(1), b"b", &m).unwrap();
    agg.apply(&b);
    let after = agg.publish();
    after.verify().unwrap();
    let first = after.node(id_by_path(&after, b"/r/a")).unwrap();
    let second = after.node(id_by_path(&after, b"/r/other/b")).unwrap();
    assert!(first.is_hard_link() && second.is_hard_link());
    assert!(!first.flags().contains(NodeFlags::HARD_LINK_DUPLICATE));
    assert!(second.flags().contains(NodeFlags::HARD_LINK_DUPLICATE));
    assert_eq!(second.hard_link_origin(), Some(first.id()));
    assert!(
        !after
            .node(id_by_path(&after, b"/r/lone"))
            .unwrap()
            .is_hard_link()
    );
    // The earlier snapshot is unchanged.
    assert!(
        !before
            .node(id_by_path(&before, b"/r/a"))
            .unwrap()
            .is_hard_link()
    );
}

#[test]
fn a_scanner_that_knows_the_link_count_can_flag_a_single_name() {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    root_batch(&mut b);
    let m = Meta {
        flags: NodeFlags::HARD_LINK,
        link: link(1, 9),
        ..sized(10, 4096)
    };
    b.file(NodeKind::File, DirKey(0), b"outside-names", &m)
        .unwrap();
    agg.apply(&b);
    let snap = agg.publish();
    let n = snap.node(id_by_path(&snap, b"/r/outside-names")).unwrap();
    assert!(n.is_hard_link());
    assert_eq!(n.allocated(), 4096, "still counted once");
    assert!(!n.flags().contains(NodeFlags::HARD_LINK_DUPLICATE));
}
