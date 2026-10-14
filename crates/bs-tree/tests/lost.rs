//! A refused or lost record must never leave the directory it belonged to
//! looking complete and exact.

mod common;

use bs_tree::{
    Aggregator, ApplierLimits, ApplyError, Batch, DeleteState, DirKey, Meta, NodeId, NodeKind,
    ReadErrorKind, ReadFailure, Snapshot,
};

fn sized(logical: u64, allocated: u64) -> Meta {
    Meta {
        logical,
        allocated,
        ..Meta::default()
    }
}

/// root(key 0) / a(1) / b(2), and root / c(3). Directories c is complete and
/// exact; the others are still open.
fn tree_with(limits: ApplierLimits) -> Aggregator {
    let mut agg = Aggregator::with_limits(limits);
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"r", &sized(0, 4096))
        .unwrap();
    b.dir(DirKey(1), DirKey(0), b"a", &sized(0, 4096)).unwrap();
    b.dir(DirKey(2), DirKey(1), b"b", &sized(0, 4096)).unwrap();
    b.dir(DirKey(3), DirKey(0), b"c", &sized(0, 4096)).unwrap();
    b.file(NodeKind::File, DirKey(3), b"cf", &sized(5, 4096))
        .unwrap();
    b.listing_done(DirKey(3));
    assert_eq!(agg.apply(&b).rejected, 0);
    agg
}

const ROOT: u32 = 0;
const A: u32 = 1;
const B: u32 = 2;
const C: u32 = 3;

fn lost(snap: &Snapshot, node: u32) -> bool {
    snap.node(NodeId::from_index(node))
        .unwrap()
        .has_lost_records_below()
}

/// Expects exactly `flagged` to have lost records below, each of them a
/// lower bound whatever its completeness, and a consistent tree.
fn expect_flagged(snap: &Snapshot, flagged: &[u32], what: &str) {
    snap.verify().unwrap_or_else(|v| panic!("{what}: {v}"));
    for node in [ROOT, A, B, C] {
        let n = snap.node(NodeId::from_index(node)).unwrap();
        assert_eq!(
            lost(snap, node),
            flagged.contains(&node),
            "{what}: node {node}"
        );
        if flagged.contains(&node) {
            assert!(n.bytes_are_lower_bound(), "{what}: node {node}");
        }
    }
    assert!(!snap.scan_complete(), "{what}");
}

fn one_record(f: impl FnOnce(&mut Batch)) -> Batch {
    let mut b = Batch::new();
    f(&mut b);
    b
}

fn refused(agg: &mut Aggregator, batch: &Batch, want: ApplyError) {
    let r = agg.apply(batch);
    assert_eq!(r.rejected, 1, "{want}");
    assert_eq!(r.first_errors[0].1, want);
}

#[test]
fn every_refusal_path_flags_the_directory_it_belonged_to() {
    // Reserved key: the parent (b) is known, so b and its ancestors are flagged.
    let mut agg = tree_with(ApplierLimits::default());
    let b = one_record(|b| {
        b.dir(DirKey::NONE, DirKey(B), b"x", &sized(0, 99)).unwrap();
    });
    refused(&mut agg, &b, ApplyError::ReservedKey);
    expect_flagged(&agg.publish(), &[ROOT, A, B], "reserved key");

    // Duplicate key: the new entry would have been a child of a.
    let mut agg = tree_with(ApplierLimits::default());
    let b = one_record(|b| {
        b.dir(DirKey(B), DirKey(A), b"dup", &sized(0, 99)).unwrap();
    });
    refused(&mut agg, &b, ApplyError::DuplicateKey(DirKey(B)));
    expect_flagged(&agg.publish(), &[ROOT, A], "duplicate key");

    // Self-parent: the directory it claims is unknown, so the root is flagged.
    let mut agg = tree_with(ApplierLimits::default());
    let b = one_record(|b| {
        b.dir(DirKey(9), DirKey(9), b"self", &sized(0, 99)).unwrap();
    });
    refused(&mut agg, &b, ApplyError::SelfParent);
    expect_flagged(&agg.publish(), &[ROOT], "self parent");

    // A second root.
    let mut agg = tree_with(ApplierLimits::default());
    let b = one_record(|b| {
        b.dir(DirKey(9), DirKey::NONE, b"root2", &sized(0, 99))
            .unwrap();
    });
    refused(&mut agg, &b, ApplyError::RootAlreadyPresent);
    expect_flagged(&agg.publish(), &[ROOT], "second root");

    // Late entry into a complete directory: c stays complete but is flagged.
    let mut agg = tree_with(ApplierLimits::default());
    let b = one_record(|b| {
        b.file(NodeKind::File, DirKey(3), b"late", &sized(9, 9_000))
            .unwrap();
    });
    refused(&mut agg, &b, ApplyError::ParentComplete);
    let snap = agg.publish();
    expect_flagged(&snap, &[ROOT, C], "parent complete");
    let c = snap.node(NodeId::from_index(C)).unwrap();
    assert!(c.is_complete());
    assert!(!c.is_in_progress());
    assert!(c.bytes_are_lower_bound(), "complete, yet a lower bound");
    // The siblings that lost nothing stay exact.
    assert!(!lost(&snap, A) && !lost(&snap, B));

    // Listing reported finished twice.
    let mut agg = tree_with(ApplierLimits::default());
    let b = one_record(|b| b.listing_done(DirKey(3)));
    refused(&mut agg, &b, ApplyError::ListingAlreadyDone);
    expect_flagged(&agg.publish(), &[ROOT, C], "listing twice");

    // Tree full.
    let mut agg = tree_with(ApplierLimits {
        max_nodes: 5,
        ..ApplierLimits::default()
    });
    let b = one_record(|b| {
        b.file(NodeKind::File, DirKey(B), b"full", &sized(1, 50_000))
            .unwrap();
    });
    refused(&mut agg, &b, ApplyError::NodeIdsExhausted);
    expect_flagged(&agg.publish(), &[ROOT, A, B], "node ids exhausted");

    // Sizes overflow.
    let mut agg = tree_with(ApplierLimits::default());
    let b = one_record(|b| {
        b.file(NodeKind::File, DirKey(B), b"huge", &sized(1, u64::MAX))
            .unwrap();
    });
    refused(&mut agg, &b, ApplyError::SizeOverflow);
    expect_flagged(&agg.publish(), &[ROOT, A, B], "size overflow");

    // Nowhere to wait: the parent is unknown, so the root is flagged.
    let mut agg = tree_with(ApplierLimits {
        max_parked_records: 0,
        ..ApplierLimits::default()
    });
    let b = one_record(|b| {
        b.file(NodeKind::File, DirKey(77), b"orphan", &sized(1, 1))
            .unwrap();
    });
    refused(&mut agg, &b, ApplyError::ParkLimit);
    expect_flagged(&agg.publish(), &[ROOT], "park limit");

    // A cycle of waiting directories: the one that closes it is refused.
    let mut agg = tree_with(ApplierLimits::default());
    let b = one_record(|b| {
        b.dir(DirKey(10), DirKey(11), b"x", &sized(0, 1)).unwrap();
        b.dir(DirKey(11), DirKey(10), b"y", &sized(0, 1)).unwrap();
    });
    refused(&mut agg, &b, ApplyError::Cycle);
    expect_flagged(&agg.publish(), &[ROOT], "cycle");

    // Anything after the scan was declared finished.
    let mut agg = tree_with(ApplierLimits::default());
    agg.finish();
    let b = one_record(|b| {
        b.file(NodeKind::File, DirKey(B), b"after", &sized(1, 1))
            .unwrap();
    });
    refused(&mut agg, &b, ApplyError::ScanFinished);
    let snap = agg.publish();
    // The directory keys are gone by then, so the root carries the flag.
    expect_flagged(&snap, &[ROOT], "after finish");
}

#[test]
fn records_still_waiting_at_finish_flag_the_root() {
    let mut agg = tree_with(ApplierLimits::default());
    let b = one_record(|b| {
        b.file(NodeKind::File, DirKey(55), b"orphan", &sized(1, 1))
            .unwrap();
        b.dir(DirKey(56), DirKey(55), b"orphan-dir", &sized(0, 1))
            .unwrap();
    });
    let r = agg.apply(&b);
    assert_eq!((r.parked, r.rejected), (2, 0));
    assert_eq!(agg.finish().unresolved, 2);
    expect_flagged(&agg.publish(), &[ROOT], "unresolved at finish");
}

#[test]
fn a_loss_before_the_root_exists_flags_the_root_when_it_arrives() {
    let mut agg = Aggregator::new();
    let b = one_record(|b| {
        b.file(NodeKind::File, DirKey::NONE, b"not-a-root", &sized(1, 1))
            .unwrap();
    });
    refused(&mut agg, &b, ApplyError::RootMustBeDirectory);
    let b = one_record(|b| {
        b.dir(DirKey(0), DirKey::NONE, b"r", &Meta::default())
            .unwrap();
        b.listing_done(DirKey(0));
    });
    assert_eq!(agg.apply(&b).rejected, 0);
    let snap = agg.publish();
    let root = snap.root().unwrap();
    assert!(root.is_complete());
    assert!(root.has_lost_records_below() && root.bytes_are_lower_bound());
}

#[test]
fn a_clean_scan_flags_nothing() {
    let mut agg = tree_with(ApplierLimits::default());
    let b = one_record(|b| {
        b.listing_done(DirKey(2));
        b.listing_done(DirKey(1));
        b.listing_done(DirKey(0));
    });
    assert_eq!(agg.apply(&b).rejected, 0);
    agg.finish();
    let snap = agg.publish();
    assert!(snap.scan_complete());
    for node in [ROOT, A, B, C] {
        let n = snap.node(NodeId::from_index(node)).unwrap();
        assert!(!n.has_lost_records_below() && !n.bytes_are_lower_bound());
    }
}

// ---- delete marks on lower-bound subtrees ---------------------------------

fn finished_tree_with_loss_in_c() -> Aggregator {
    let mut agg = tree_with(ApplierLimits::default());
    let b = one_record(|b| {
        b.listing_done(DirKey(2));
        b.listing_done(DirKey(1));
        b.listing_done(DirKey(0));
        // Lost: c was already complete.
        b.file(NodeKind::File, DirKey(3), b"late", &sized(9, 9_000))
            .unwrap();
    });
    assert_eq!(agg.apply(&b).rejected, 1);
    agg
}

#[test]
fn marking_a_lower_bound_subtree_says_so_and_undo_restores_exactly() {
    let mut agg = finished_tree_with_loss_in_c();
    let base = agg.publish();
    let (c, a) = (NodeId::from_index(C), NodeId::from_index(A));

    // The exact sibling is exact.
    let exact = agg.mark_deleted(a, DeleteState::InBin).unwrap();
    assert!(!exact.lower_bound);
    let snap = agg.publish();
    assert!(!snap.root().unwrap().delete_totals().is_lower_bound());

    // The flagged one is a lower bound, and so are the totals above it.
    let moved = agg.mark_deleted(c, DeleteState::Deleted).unwrap();
    assert!(moved.lower_bound);
    assert_eq!(moved.bytes.allocated, 4096 + 4096);
    let snap = agg.publish();
    snap.verify().unwrap();
    assert!(snap.node(c).unwrap().delete_totals().is_lower_bound());
    assert!(snap.root().unwrap().delete_totals().is_lower_bound());
    assert!(
        !snap.node(a).unwrap().delete_totals().is_lower_bound(),
        "a's own totals do not include c"
    );

    // Undo returns the same verdict and removes it from the totals.
    assert!(agg.unmark_deleted(c).unwrap().lower_bound);
    let snap = agg.publish();
    assert!(!snap.root().unwrap().delete_totals().is_lower_bound());
    assert!(!agg.unmark_deleted(a).unwrap().lower_bound);
    let end = agg.publish();
    assert_eq!(end.root().unwrap().delete_totals(), Default::default());
    assert_eq!(
        end.root().unwrap().current_allocated(),
        base.root().unwrap().current_allocated()
    );
}

#[test]
fn a_subtree_that_becomes_a_lower_bound_after_it_was_marked_is_reported_as_one() {
    let mut agg = tree_with(ApplierLimits::default());
    let c = NodeId::from_index(C);
    let moved = agg.mark_deleted(c, DeleteState::InBin).unwrap();
    assert!(!moved.lower_bound);
    let snap = agg.publish();
    assert!(!snap.root().unwrap().delete_totals().is_lower_bound());

    // A late record for c is refused afterwards.
    let b = one_record(|b| {
        b.file(NodeKind::File, DirKey(3), b"late", &sized(9, 9_000))
            .unwrap();
    });
    refused(&mut agg, &b, ApplyError::ParentComplete);
    let snap = agg.publish();
    snap.verify().unwrap();
    assert!(snap.node(c).unwrap().delete_totals().is_lower_bound());
    assert!(snap.root().unwrap().delete_totals().is_lower_bound());

    assert!(agg.unmark_deleted(c).unwrap().lower_bound);
    let snap = agg.publish();
    assert_eq!(snap.root().unwrap().delete_totals(), Default::default());
    assert_eq!(snap.node(c).unwrap().delete_totals(), Default::default());
}

#[test]
fn unreadable_parts_make_a_mark_a_lower_bound_too() {
    let mut agg = Aggregator::new();
    let b = one_record(|b| {
        b.dir(DirKey(0), DirKey::NONE, b"r", &sized(0, 4096))
            .unwrap();
        b.dir(DirKey(1), DirKey(0), b"locked", &sized(0, 4096))
            .unwrap();
        b.unreadable(
            DirKey(1),
            ReadFailure {
                kind: ReadErrorKind::PermissionDenied,
                code: 13,
            },
        );
        b.listing_done(DirKey(0));
    });
    assert_eq!(agg.apply(&b).rejected, 0);
    let moved = agg
        .mark_deleted(NodeId::ROOT, DeleteState::InBin)
        .expect("complete");
    assert!(moved.lower_bound);
    assert!(
        agg.publish()
            .root()
            .unwrap()
            .delete_totals()
            .is_lower_bound()
    );
    agg.unmark_deleted(NodeId::ROOT).unwrap();
    assert_eq!(
        agg.publish().root().unwrap().delete_totals(),
        Default::default()
    );
}
