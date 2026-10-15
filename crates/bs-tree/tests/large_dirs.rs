//! The large-folder query: the answer is what a walk of every node gives,
//! and the search prunes what cannot qualify.

use bs_tree::{
    Aggregator, Batch, DeleteState, DirKey, LargeDir, LargeDirs, Meta, NodeFlags, NodeId, NodeKind,
    Snapshot,
};
use proptest::prelude::*;

fn sized(allocated: u64) -> Meta {
    Meta {
        allocated,
        logical: allocated,
        ..Meta::default()
    }
}

/// `/` / { big (dirs and files, 300 + …), small, mid / { inner (150) , x (60) } }.
fn sample() -> (Aggregator, std::sync::Arc<Snapshot>) {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"/", &sized(0)).unwrap();
    b.dir(DirKey(1), DirKey(0), b"big", &sized(0)).unwrap();
    b.file(NodeKind::File, DirKey(1), b"f", &sized(300))
        .unwrap();
    b.listing_done(DirKey(1));
    b.dir(DirKey(2), DirKey(0), b"small", &sized(0)).unwrap();
    b.file(NodeKind::File, DirKey(2), b"f", &sized(10)).unwrap();
    b.listing_done(DirKey(2));
    b.dir(DirKey(3), DirKey(0), b"mid", &sized(0)).unwrap();
    b.dir(DirKey(4), DirKey(3), b"inner", &sized(0)).unwrap();
    b.file(NodeKind::File, DirKey(4), b"f", &sized(150))
        .unwrap();
    b.listing_done(DirKey(4));
    b.dir(DirKey(5), DirKey(3), b"x", &sized(0)).unwrap();
    b.file(NodeKind::File, DirKey(5), b"f", &sized(60)).unwrap();
    b.listing_done(DirKey(5));
    b.listing_done(DirKey(3));
    b.listing_done(DirKey(0));
    assert_eq!(agg.apply(&b).rejected, 0);
    agg.finish();
    let snap = agg.publish();
    (agg, snap)
}

fn names(snap: &Snapshot, found: &LargeDirs) -> Vec<String> {
    found
        .dirs
        .iter()
        .map(|d| String::from_utf8(snap.path(d.id, b'/').unwrap()).unwrap())
        .collect()
}

#[test]
fn finds_the_directories_at_or_over_the_size_largest_first() {
    let (_agg, snap) = sample();
    let found = snap.large_dirs(NodeId::ROOT, 100, 99, 10).unwrap();
    assert_eq!(names(&snap, &found), ["/big", "/mid", "/mid/inner"]);
    assert_eq!(
        found
            .dirs
            .iter()
            .map(|d| (d.allocated, d.depth))
            .collect::<Vec<_>>(),
        [(300, 1), (210, 1), (150, 2)]
    );
    assert!(!found.more);
    // The size is a floor that includes the boundary.
    let exact = snap.large_dirs(NodeId::ROOT, 150, 99, 10).unwrap();
    assert_eq!(exact.dirs.len(), 3);
    let over = snap.large_dirs(NodeId::ROOT, 151, 99, 10).unwrap();
    assert_eq!(over.dirs.len(), 2);
    let none = snap.large_dirs(NodeId::ROOT, 301, 99, 10).unwrap();
    assert!(none.dirs.is_empty() && !none.more);
}

#[test]
fn depth_counts_from_where_the_search_starts() {
    let (_agg, snap) = sample();
    assert_eq!(
        snap.large_dirs(NodeId::ROOT, 0, 0, 10).unwrap().dirs.len(),
        0
    );
    let one = snap.large_dirs(NodeId::ROOT, 0, 1, 10).unwrap();
    assert_eq!(one.dirs.len(), 3);
    assert!(one.dirs.iter().all(|d| d.depth == 1));
    let two = snap.large_dirs(NodeId::ROOT, 0, 2, 10).unwrap();
    assert_eq!(two.dirs.len(), 5);
    // From a folder below the root, its children are depth 1.
    let mid = snap
        .root()
        .unwrap()
        .children()
        .find(|c| c.name() == b"mid")
        .unwrap()
        .id();
    let below = snap.large_dirs(mid, 0, 99, 10).unwrap();
    assert_eq!(
        below.dirs.iter().map(|d| d.depth).collect::<Vec<_>>(),
        [1, 1]
    );
    // The folder asked about is not in its own answer.
    assert!(below.dirs.iter().all(|d| d.id != mid));
}

#[test]
fn the_limit_keeps_the_largest_and_says_more_were_left_out() {
    let (_agg, snap) = sample();
    let top = snap.large_dirs(NodeId::ROOT, 0, 99, 2).unwrap();
    assert_eq!(names(&snap, &top), ["/big", "/mid"]);
    assert!(top.more);
    let all = snap.large_dirs(NodeId::ROOT, 0, 99, 5).unwrap();
    assert_eq!(all.dirs.len(), 5);
    assert!(!all.more);
    let zero = snap.large_dirs(NodeId::ROOT, 0, 99, 0).unwrap();
    assert!(zero.dirs.is_empty() && zero.more);
}

#[test]
fn equal_sizes_are_told_apart_by_node_id() {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"/", &sized(0)).unwrap();
    for k in 1..=4 {
        b.dir(
            DirKey(k),
            DirKey(0),
            format!("d{k}").as_bytes(),
            &sized(100),
        )
        .unwrap();
        b.listing_done(DirKey(k));
    }
    b.listing_done(DirKey(0));
    agg.apply(&b);
    agg.finish();
    let snap = agg.publish();
    let two = snap.large_dirs(NodeId::ROOT, 0, 1, 2).unwrap();
    let ids: Vec<u32> = two.dirs.iter().map(|d| d.id.index()).collect();
    assert_eq!(ids, [1, 2]);
    assert!(two.more);
}

#[test]
fn deleted_folders_and_what_is_below_them_are_not_candidates() {
    let (mut agg, snap) = sample();
    let mid = snap
        .root()
        .unwrap()
        .children()
        .find(|c| c.name() == b"mid")
        .unwrap()
        .id();
    agg.mark_deleted(mid, DeleteState::InBin).unwrap();
    let snap = agg.publish();
    let found = snap.large_dirs(NodeId::ROOT, 0, 99, 10).unwrap();
    assert_eq!(names(&snap, &found), ["/big", "/small"]);
    // Asking from inside the marked folder still works: the folder asked
    // about is the caller's choice.
    assert!(snap.large_dirs(mid, 0, 99, 10).is_some());
}

#[test]
fn made_up_folders_are_not_candidates() {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"/", &sized(0)).unwrap();
    let synthetic = Meta {
        flags: NodeFlags::SYNTHETIC,
        ..sized(0)
    };
    b.dir(DirKey(1), DirKey(0), b"<strays>", &synthetic)
        .unwrap();
    b.file(NodeKind::File, DirKey(1), b"f", &sized(500))
        .unwrap();
    b.listing_done(DirKey(1));
    b.listing_done(DirKey(0));
    agg.apply(&b);
    agg.finish();
    let snap = agg.publish();
    assert!(
        snap.large_dirs(NodeId::ROOT, 0, 99, 10)
            .unwrap()
            .dirs
            .is_empty()
    );
}

#[test]
fn unknown_nodes_have_no_answer_and_files_have_an_empty_one() {
    let (_agg, snap) = sample();
    assert_eq!(snap.large_dirs(NodeId::from_index(500), 0, 9, 9), None);
    let file = snap
        .root()
        .unwrap()
        .children()
        .find(|c| c.name() == b"big")
        .unwrap()
        .children()
        .next()
        .unwrap()
        .id();
    assert!(snap.large_dirs(file, 0, 9, 9).unwrap().dirs.is_empty());
}

#[test]
fn a_tree_still_being_scanned_answers_with_what_is_counted() {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"/", &sized(0)).unwrap();
    b.dir(DirKey(1), DirKey(0), b"a", &sized(0)).unwrap();
    b.file(NodeKind::File, DirKey(1), b"f", &sized(70)).unwrap();
    agg.apply(&b);
    let snap = agg.publish();
    let found = snap.large_dirs(NodeId::ROOT, 50, 9, 9).unwrap();
    assert_eq!(found.dirs.len(), 1);
    assert!(snap.node(found.dirs[0].id).unwrap().is_in_progress());
}

// ---- against a walk of every node ------------------------------------------------------

fn brute(
    snap: &Snapshot,
    from: NodeId,
    min: u64,
    depth: u32,
    limit: usize,
) -> (Vec<LargeDir>, bool) {
    let mut all = Vec::new();
    let mut stack = vec![(from, 0u32)];
    while let Some((id, d)) = stack.pop() {
        let node = snap.node(id).unwrap();
        for c in node.children() {
            if c.kind() == NodeKind::Dir && d < depth {
                if c.allocated() >= min && c.delete_state() == DeleteState::None {
                    all.push(LargeDir {
                        id: c.id(),
                        depth: d + 1,
                        allocated: c.allocated(),
                    });
                }
                if c.delete_state() == DeleteState::None {
                    stack.push((c.id(), d + 1));
                }
            }
        }
    }
    all.sort_by(|a, b| b.allocated.cmp(&a.allocated).then(a.id.cmp(&b.id)));
    let more = all.len() > limit;
    all.truncate(limit);
    (all, more)
}

proptest! {
    #[test]
    fn the_answer_is_what_a_walk_of_every_node_gives(
        raw in proptest::collection::vec((any::<u8>(), proptest::bool::weighted(0.4), 0u16..2000), 0..80),
        min in 0u64..3000,
        depth in 0u32..6,
        limit in 0usize..12,
        marks in proptest::collection::vec(any::<prop::sample::Index>(), 0..3),
    ) {
        let mut agg = Aggregator::new();
        let mut b = Batch::new();
        b.dir(DirKey(0), DirKey::NONE, b"/", &sized(0)).unwrap();
        let mut dirs = vec![0u32];
        let mut next = 1u32;
        for &(pick, is_dir, size) in &raw {
            let parent = dirs[usize::from(pick) % dirs.len()];
            if is_dir {
                b.dir(DirKey(next), DirKey(parent), b"d", &sized(0)).unwrap();
                dirs.push(next);
                next += 1;
            } else {
                b.file(NodeKind::File, DirKey(parent), b"f", &sized(u64::from(size))).unwrap();
            }
        }
        for &d in &dirs {
            b.listing_done(DirKey(d));
        }
        prop_assert_eq!(agg.apply(&b).rejected, 0);
        agg.finish();
        let snap = agg.publish();
        // Directory keys were handed out in node order, so key == node id.
        for m in marks {
            let id = NodeId::from_index(dirs[m.index(dirs.len())]);
            if id != NodeId::ROOT {
                let _ = agg.mark_deleted(id, DeleteState::InBin);
            }
        }
        drop(snap);
        let snap = agg.publish();
        let got = snap.large_dirs(NodeId::ROOT, min, depth, limit).unwrap();
        let (want, more) = brute(&snap, NodeId::ROOT, min, depth, limit);
        prop_assert_eq!(got.dirs, want);
        prop_assert_eq!(got.more, more);
    }
}
