//! Delete marks: ancestors expose current and moved-to-bin bytes, nothing
//! moves, undo restores everything exactly.

mod common;

use bs_tree::{
    Aggregator, Batch, ByteCount, DeleteError, DeleteState, DirKey, Meta, NodeId, NodeKind,
    Snapshot,
};
use common::*;
use std::sync::Arc;

fn sized(logical: u64, allocated: u64) -> Meta {
    Meta {
        logical,
        allocated,
        ..Meta::default()
    }
}

/// root(4096) / a(4096) / { f1 (100, 4096), b(4096) / { f2 (10, 8192) } }, g (5, 4096)
fn sample() -> Aggregator {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"r", &sized(0, 4096))
        .unwrap();
    b.dir(DirKey(1), DirKey(0), b"a", &sized(0, 4096)).unwrap();
    b.file(NodeKind::File, DirKey(1), b"f1", &sized(100, 4096))
        .unwrap();
    b.dir(DirKey(2), DirKey(1), b"b", &sized(0, 4096)).unwrap();
    b.file(NodeKind::File, DirKey(2), b"f2", &sized(10, 8192))
        .unwrap();
    b.file(NodeKind::File, DirKey(0), b"g", &sized(5, 4096))
        .unwrap();
    b.listing_done(DirKey(2));
    b.listing_done(DirKey(1));
    b.listing_done(DirKey(0));
    agg.apply(&b);
    agg
}

fn find(snap: &Snapshot, name: &[u8]) -> NodeId {
    (0..snap.len())
        .map(NodeId::from_index)
        .find(|&id| snap.node(id).unwrap().name() == name)
        .unwrap()
}

/// Everything observable about every node, to compare snapshots exactly.
fn fingerprint(snap: &Snapshot) -> Vec<String> {
    (0..snap.len())
        .map(|i| {
            let n = snap.node(NodeId::from_index(i)).unwrap();
            format!(
                "{:?} {} {} {} {} {} {:?}",
                n.name(),
                n.current_allocated(),
                n.allocated(),
                n.file_count(),
                n.delete_totals().in_bin.allocated,
                n.delete_totals().deleted.allocated,
                n.delete_state(),
            )
        })
        .collect()
}

#[test]
fn moved_to_bin_shows_on_every_ancestor_and_changes_no_counted_size() {
    let mut agg = sample();
    let before = agg.publish();
    let b = find(&before, b"b");
    let moved = agg.mark_deleted(b, DeleteState::InBin).unwrap();
    assert_eq!(
        moved.bytes,
        ByteCount {
            logical: 10,
            allocated: 4096 + 8192,
            files: 1
        }
    );
    assert!(!moved.lower_bound, "a fully counted subtree is exact");
    let after = agg.publish();
    after.verify().unwrap();

    // Structure and counted sizes are untouched.
    assert_eq!(before.len(), after.len());
    for i in 0..before.len() {
        let (x, y) = (
            before.node(NodeId::from_index(i)).unwrap(),
            after.node(NodeId::from_index(i)).unwrap(),
        );
        assert_eq!(
            (x.allocated(), x.logical(), x.file_count()),
            (y.allocated(), y.logical(), y.file_count())
        );
        assert_eq!(x.parent().map(|p| p.id()), y.parent().map(|p| p.id()));
    }
    for name in [&b"b"[..], b"a", b"r"] {
        let n = after.node(find(&after, name)).unwrap();
        assert_eq!(n.delete_totals().in_bin.allocated, 12288, "{name:?}");
        assert_eq!(n.delete_totals().in_bin.logical, 10);
        assert_eq!(n.delete_totals().in_bin.files, 1);
        assert_eq!(n.delete_totals().deleted, ByteCount::default());
    }
    let r = after.root().unwrap();
    assert_eq!(r.allocated(), 4096 * 4 + 4096 + 8192);
    assert_eq!(r.current_allocated(), 4096 * 4);
    assert_eq!(r.current_logical(), 105);
    assert_eq!(r.current_files(), 2);
    // Siblings are unaffected.
    let g = after.node(find(&after, b"g")).unwrap();
    assert_eq!(g.delete_totals(), Default::default());
    assert_eq!(g.current_allocated(), 4096);
    // The marked node's descendants report the state they are in.
    let f2 = after.node(find(&after, b"f2")).unwrap();
    assert_eq!(f2.delete_state(), DeleteState::None);
    assert_eq!(f2.effective_delete_state(), DeleteState::InBin);
    assert_eq!(after.node(b).unwrap().delete_state(), DeleteState::InBin);
    assert_eq!(after.node(b).unwrap().current_allocated(), 0);
}

#[test]
fn permanent_delete_is_tracked_separately_from_the_bin() {
    let mut agg = sample();
    let snap = agg.publish();
    agg.mark_deleted(find(&snap, b"f1"), DeleteState::InBin)
        .unwrap();
    agg.mark_deleted(find(&snap, b"g"), DeleteState::Deleted)
        .unwrap();
    let snap = agg.publish();
    snap.verify().unwrap();
    let r = snap.root().unwrap();
    assert_eq!(r.delete_totals().in_bin.allocated, 4096);
    assert_eq!(r.delete_totals().deleted.allocated, 4096);
    assert_eq!(r.delete_totals().total().allocated, 8192);
    assert_eq!(r.delete_totals().deleted.logical, 5);
    assert_eq!(r.current_allocated(), r.allocated() - 8192);
    let a = snap.node(find(&snap, b"a")).unwrap();
    assert_eq!(a.delete_totals().deleted, ByteCount::default());
}

#[test]
fn undo_restores_every_total_exactly() {
    let mut agg = sample();
    let original = agg.publish();
    let base = fingerprint(&original);
    let (a, f1, g) = (
        find(&original, b"a"),
        find(&original, b"f1"),
        find(&original, b"g"),
    );

    agg.mark_deleted(f1, DeleteState::InBin).unwrap();
    agg.mark_deleted(g, DeleteState::Deleted).unwrap();
    let mid = agg.publish();
    assert_ne!(fingerprint(&mid), base);
    // Marking an ancestor covers only what is not already marked.
    let moved = agg.mark_deleted(a, DeleteState::InBin).unwrap();
    assert_eq!(
        moved.bytes.allocated,
        4096 + 4096 + 8192,
        "a, b, f2 but not f1"
    );
    let nested = agg.publish();
    nested.verify().unwrap();
    assert_eq!(nested.node(a).unwrap().current_allocated(), 0);
    assert_eq!(
        nested.root().unwrap().delete_totals().in_bin.allocated,
        4096 * 5
    );

    // Undo in reverse order of marking.
    assert_eq!(agg.unmark_deleted(a).unwrap(), moved);
    assert_eq!(fingerprint(&agg.publish()), fingerprint(&mid));
    agg.unmark_deleted(g).unwrap();
    agg.unmark_deleted(f1).unwrap();
    let restored = agg.publish();
    restored.verify().unwrap();
    assert_eq!(fingerprint(&restored), base);
    assert_eq!(restored.root().unwrap().delete_totals(), Default::default());
}

#[test]
fn undo_is_exact_in_any_order_when_no_ancestor_is_marked() {
    let mut agg = sample();
    let snap = agg.publish();
    let base = fingerprint(&snap);
    let names: [&[u8]; 3] = [b"f1", b"f2", b"g"];
    let ids: Vec<NodeId> = names.iter().map(|n| find(&snap, n)).collect();
    for &id in &ids {
        agg.mark_deleted(id, DeleteState::InBin).unwrap();
    }
    // No ancestor is marked, so any order restores the same totals.
    for &i in &[2usize, 0, 1] {
        agg.unmark_deleted(ids[i]).unwrap();
    }
    assert_eq!(fingerprint(&agg.publish()), base);
}

#[test]
fn refusals_leave_everything_unchanged() {
    let mut agg = sample();
    let snap = agg.publish();
    let base = fingerprint(&snap);
    let (a, f2) = (find(&snap, b"a"), find(&snap, b"f2"));
    assert_eq!(
        agg.mark_deleted(NodeId::from_index(500), DeleteState::InBin),
        Err(DeleteError::UnknownNode(NodeId::from_index(500)))
    );
    assert_eq!(
        agg.mark_deleted(a, DeleteState::None),
        Err(DeleteError::NoState)
    );
    assert_eq!(agg.unmark_deleted(a), Err(DeleteError::NotMarked(a)));
    agg.mark_deleted(a, DeleteState::InBin).unwrap();
    assert_eq!(
        agg.mark_deleted(a, DeleteState::Deleted),
        Err(DeleteError::AlreadyMarked(a))
    );
    assert_eq!(
        agg.mark_deleted(f2, DeleteState::InBin),
        Err(DeleteError::AncestorMarked(f2))
    );
    agg.unmark_deleted(a).unwrap();
    assert_eq!(fingerprint(&agg.publish()), base);

    // Undoing a child while its ancestor is marked is refused too.
    agg.mark_deleted(f2, DeleteState::InBin).unwrap();
    agg.mark_deleted(a, DeleteState::InBin).unwrap();
    let both = fingerprint(&agg.publish());
    assert_eq!(agg.unmark_deleted(f2), Err(DeleteError::AncestorMarked(f2)));
    assert_eq!(fingerprint(&agg.publish()), both);
}

#[test]
fn only_complete_subtrees_can_be_marked() {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"r", &Meta::default())
        .unwrap();
    b.dir(DirKey(1), DirKey(0), b"busy", &Meta::default())
        .unwrap();
    b.file(NodeKind::File, DirKey(0), b"done", &sized(1, 1))
        .unwrap();
    agg.apply(&b);
    let snap = agg.publish();
    let (busy, done) = (find(&snap, b"busy"), find(&snap, b"done"));
    assert_eq!(
        agg.mark_deleted(busy, DeleteState::InBin),
        Err(DeleteError::NotComplete(busy))
    );
    assert_eq!(
        agg.mark_deleted(NodeId::ROOT, DeleteState::InBin),
        Err(DeleteError::NotComplete(NodeId::ROOT))
    );
    // A finished file inside an unfinished directory may be acted on.
    assert!(agg.mark_deleted(done, DeleteState::InBin).is_ok());
}

#[test]
fn old_snapshots_do_not_see_later_marks() {
    let mut agg = sample();
    let before = agg.publish();
    let f1 = find(&before, b"f1");
    agg.mark_deleted(f1, DeleteState::Deleted).unwrap();
    let after = agg.publish();
    assert_eq!(before.node(f1).unwrap().delete_state(), DeleteState::None);
    assert_eq!(before.root().unwrap().delete_totals(), Default::default());
    assert_eq!(after.node(f1).unwrap().delete_state(), DeleteState::Deleted);
    assert!(Arc::strong_count(&before) >= 1);
}

#[test]
fn random_marks_and_undos_keep_the_invariants_and_return_to_the_start() {
    let raw: Vec<(u32, bool, u16, u16)> = (0..300u32)
        .map(|i| {
            (
                i.wrapping_mul(40503) >> 3,
                i % 5 == 0,
                (i * 13 % 700) as u16,
                (i % 7) as u16,
            )
        })
        .collect();
    let spec = spec_from_raw(&raw);
    for seed in 1..=10u64 {
        let mut agg = Aggregator::new();
        for b in batches_for(&spec) {
            agg.apply(&b);
        }
        let start = agg.publish();
        assert!(start.scan_complete());
        let base = fingerprint(&start);
        let mut rng = Rng(seed.wrapping_mul(0x2545_F491_4F6C_DD1D));
        let mut marked: Vec<NodeId> = Vec::new();
        for _ in 0..400 {
            if rng.below(3) != 0 || marked.is_empty() {
                let id = NodeId::from_index(rng.below(start.len() as usize) as u32);
                let state = if rng.below(2) == 0 {
                    DeleteState::InBin
                } else {
                    DeleteState::Deleted
                };
                if agg.mark_deleted(id, state).is_ok() {
                    marked.push(id);
                }
            } else {
                // Undo the most recent mark (the way the UI's undo works).
                let id = marked.pop().unwrap();
                agg.unmark_deleted(id).unwrap();
            }
            let snap = agg.publish();
            snap.verify().unwrap();
            let r = snap.root().unwrap();
            assert!(r.current_allocated() <= r.allocated());
            assert_eq!(r.allocated(), start.root().unwrap().allocated());
        }
        while let Some(id) = marked.pop() {
            agg.unmark_deleted(id).unwrap();
        }
        assert_eq!(fingerprint(&agg.publish()), base, "seed {seed}");
    }
}

/// root / { dir / { guess (estimated), exact }, solo (exact), other (estimated) }
///
/// `guess` is the only estimated entry under `dir`; `other` sits next to it.
fn with_estimates() -> Aggregator {
    let estimate = |logical, allocated| Meta {
        flags: bs_tree::NodeFlags::ESTIMATED_SIZE,
        ..sized(logical, allocated)
    };
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"r", &sized(0, 4096))
        .unwrap();
    b.dir(DirKey(1), DirKey(0), b"dir", &sized(0, 4096))
        .unwrap();
    b.file(NodeKind::File, DirKey(1), b"guess", &estimate(1, 4096))
        .unwrap();
    b.file(NodeKind::File, DirKey(1), b"exact", &sized(10, 8192))
        .unwrap();
    b.file(NodeKind::File, DirKey(0), b"solo", &sized(5, 4096))
        .unwrap();
    b.file(NodeKind::File, DirKey(0), b"other", &estimate(2, 4096))
        .unwrap();
    b.listing_done(DirKey(1));
    b.listing_done(DirKey(0));
    agg.apply(&b);
    agg
}

#[test]
fn deleting_an_estimated_file_says_estimated_on_it_and_every_ancestor() {
    let mut agg = with_estimates();
    let snap = agg.publish();
    let guess = find(&snap, b"guess");
    let moved = agg.mark_deleted(guess, DeleteState::InBin).unwrap();
    assert!(moved.estimated);
    assert!(!moved.lower_bound, "an estimate is not a lower bound");
    let after = agg.publish();
    after.verify().unwrap();
    for name in [&b"guess"[..], b"dir", b"r"] {
        let totals = after.node(find(&after, name)).unwrap().delete_totals();
        assert!(totals.is_estimated(), "{name:?}");
        assert!(!totals.is_lower_bound(), "{name:?}");
    }
    // Siblings and the estimated entry nobody moved are untouched.
    for name in [&b"exact"[..], b"solo", b"other"] {
        let totals = after.node(find(&after, name)).unwrap().delete_totals();
        assert!(!totals.is_estimated(), "{name:?}");
    }
    // Undo says the same thing and removes it.
    assert_eq!(agg.unmark_deleted(guess).unwrap(), moved);
    let undone = agg.publish();
    assert_eq!(undone.root().unwrap().delete_totals(), Default::default());
    assert!(!undone.root().unwrap().delete_totals().is_estimated());
}

#[test]
fn deleting_an_exact_file_is_exact_even_beside_an_estimate() {
    let mut agg = with_estimates();
    let snap = agg.publish();
    // `exact` shares its folder with an estimated file, and `solo` shares the
    // root with one; neither mark moves an estimated byte.
    for name in [&b"exact"[..], b"solo"] {
        let moved = agg
            .mark_deleted(find(&snap, name), DeleteState::Deleted)
            .unwrap();
        assert!(!moved.estimated, "{name:?}");
    }
    let after = agg.publish();
    after.verify().unwrap();
    for name in [&b"exact"[..], b"dir", b"solo", b"r"] {
        let totals = after.node(find(&after, name)).unwrap().delete_totals();
        assert!(!totals.is_estimated(), "{name:?}");
    }
    // The folder's own size still says it contains a guess.
    assert!(
        after
            .node(find(&after, b"dir"))
            .unwrap()
            .size_is_estimated()
    );
}

#[test]
fn mixed_deletes_are_estimated_only_while_an_estimated_mark_stands() {
    let mut agg = with_estimates();
    let snap = agg.publish();
    let (exact, guess, solo) = (
        find(&snap, b"exact"),
        find(&snap, b"guess"),
        find(&snap, b"solo"),
    );
    let base = fingerprint(&snap);
    agg.mark_deleted(solo, DeleteState::InBin).unwrap();
    agg.mark_deleted(exact, DeleteState::Deleted).unwrap();
    let both_exact = agg.publish();
    assert!(!both_exact.root().unwrap().delete_totals().is_estimated());

    agg.mark_deleted(guess, DeleteState::InBin).unwrap();
    let mixed = agg.publish();
    mixed.verify().unwrap();
    let root = mixed.root().unwrap().delete_totals();
    assert!(root.is_estimated(), "one estimated mark among exact ones");
    let dir = mixed.node(find(&mixed, b"dir")).unwrap().delete_totals();
    assert!(dir.is_estimated());
    // The bytes of each state are still the plain sums.
    assert_eq!(root.in_bin.allocated, 4096 + 4096);
    assert_eq!(root.deleted.allocated, 8192);
    // The guess is in the bin; what was deleted is exact.
    assert!(root.in_bin_is_estimated());
    assert!(!root.deleted_is_estimated());

    // Undo the estimated mark: what is left is exact again.
    agg.unmark_deleted(guess).unwrap();
    let after_undo = agg.publish();
    assert!(!after_undo.root().unwrap().delete_totals().is_estimated());
    assert_eq!(
        after_undo.root().unwrap().delete_totals().total().allocated,
        4096 + 8192
    );
    agg.unmark_deleted(exact).unwrap();
    agg.unmark_deleted(solo).unwrap();
    assert_eq!(fingerprint(&agg.publish()), base);
}

#[test]
fn marking_a_folder_over_an_already_marked_estimate_stays_on_the_safe_side() {
    // The folder's flag says it holds a guess, and the guess was moved by an
    // earlier mark of its own, so the folder's mark covers exact bytes only.
    // The answer is still "estimated": it may say so for exact bytes but never
    // the reverse. Undoing both leaves nothing behind.
    let mut agg = with_estimates();
    let snap = agg.publish();
    let (dir, guess) = (find(&snap, b"dir"), find(&snap, b"guess"));
    agg.mark_deleted(guess, DeleteState::InBin).unwrap();
    let outer = agg.mark_deleted(dir, DeleteState::InBin).unwrap();
    assert!(outer.estimated);
    let both = agg.publish();
    both.verify().unwrap();
    assert!(both.root().unwrap().delete_totals().is_estimated());
    agg.unmark_deleted(dir).unwrap();
    agg.unmark_deleted(guess).unwrap();
    let restored = agg.publish();
    assert_eq!(restored.root().unwrap().delete_totals(), Default::default());
}

#[test]
fn an_estimate_and_a_lower_bound_are_tracked_apart() {
    // `bad` could not be read (a lower bound, exact as far as it goes);
    // `guess` was read but its allocated size is an estimate.
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"r", &sized(0, 4096))
        .unwrap();
    b.dir(DirKey(1), DirKey(0), b"bad", &sized(0, 4096))
        .unwrap();
    b.unreadable(
        DirKey(1),
        bs_tree::ReadFailure {
            kind: bs_tree::ReadErrorKind::PermissionDenied,
            code: 13,
        },
    );
    b.file(
        NodeKind::File,
        DirKey(0),
        b"guess",
        &Meta {
            flags: bs_tree::NodeFlags::ESTIMATED_SIZE,
            ..sized(1, 4096)
        },
    )
    .unwrap();
    b.listing_done(DirKey(0));
    agg.apply(&b);
    let snap = agg.publish();
    let bad = agg
        .mark_deleted(find(&snap, b"bad"), DeleteState::InBin)
        .unwrap();
    assert!(bad.lower_bound && !bad.estimated);
    let guess = agg
        .mark_deleted(find(&snap, b"guess"), DeleteState::InBin)
        .unwrap();
    assert!(guess.estimated && !guess.lower_bound);
    let after = agg.publish();
    after.verify().unwrap();
    let root = after.root().unwrap().delete_totals();
    assert!(root.is_lower_bound() && root.is_estimated());
    let only_bad = after.node(find(&after, b"bad")).unwrap().delete_totals();
    assert!(only_bad.is_lower_bound() && !only_bad.is_estimated());
    let only_guess = after.node(find(&after, b"guess")).unwrap().delete_totals();
    assert!(only_guess.is_estimated() && !only_guess.is_lower_bound());
}
