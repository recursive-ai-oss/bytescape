//! Pathologically deep and long-chained input must stay cheap.

use bs_tree::{Aggregator, Batch, DirKey, Meta, NodeId, NodeKind};
use std::time::{Duration, Instant};

fn sized(allocated: u64) -> Meta {
    Meta {
        logical: allocated / 2,
        allocated,
        ..Meta::default()
    }
}

/// Generous in debug builds, tight in release builds.
fn limit() -> Duration {
    if cfg!(debug_assertions) {
        Duration::from_secs(20)
    } else {
        Duration::from_secs(1)
    }
}

const DEPTH: u32 = 60_000;

/// A chain of `DEPTH` nested directories with one file at the bottom, fed in
/// batches of `per_batch` records.
fn deep_chain(per_batch: u32) -> (Aggregator, Duration) {
    let mut agg = Aggregator::new();
    let start = Instant::now();
    let mut batch = Batch::new();
    batch
        .dir(DirKey(0), DirKey::NONE, b"/", &sized(4096))
        .unwrap();
    for key in 1..=DEPTH {
        batch
            .dir(DirKey(key), DirKey(key - 1), b"d", &sized(4096))
            .unwrap();
        if batch.len() as u32 >= per_batch {
            assert_eq!(agg.apply(&batch).rejected, 0);
            batch.clear();
        }
    }
    batch
        .file(NodeKind::File, DirKey(DEPTH), b"leaf", &sized(8192))
        .unwrap();
    // Close from the bottom up: the last one completes the whole chain.
    for key in (0..=DEPTH).rev() {
        batch.listing_done(DirKey(key));
    }
    assert_eq!(agg.apply(&batch).rejected, 0);
    (agg, start.elapsed())
}

fn check_chain(agg: &mut Aggregator) {
    let snap = agg.publish();
    let root = snap.root().unwrap();
    assert_eq!(snap.len(), DEPTH + 2);
    assert_eq!(root.allocated(), 4096 * u64::from(DEPTH + 1) + 8192);
    assert_eq!(root.file_count(), 1);
    assert!(snap.scan_complete());
    let leaf = snap.node(NodeId::from_index(DEPTH + 1)).unwrap();
    assert_eq!(leaf.allocated(), 8192);
    snap.verify().unwrap();
}

#[test]
fn a_deep_chain_in_one_batch_is_applied_in_linear_time() {
    let (mut agg, took) = deep_chain(u32::MAX);
    assert!(took < limit(), "took {took:?}");
    check_chain(&mut agg);
}

#[test]
fn a_deep_chain_in_many_batches_is_applied_in_reasonable_time() {
    // Each batch settles the whole chain above it once, so cost grows with
    // the number of batches times the depth: 60k / 4096 batches here.
    let (mut agg, took) = deep_chain(4096);
    assert!(took < limit(), "took {took:?}");
    check_chain(&mut agg);
}

#[test]
fn a_long_chain_of_waiting_directories_is_cheap_to_check_and_drains_iteratively() {
    let mut agg = Aggregator::new();
    let n: u32 = 100_000;
    let start = Instant::now();
    // Directory i waits for directory i + 1, nothing defined yet. Fed in
    // both orders, which are the awkward ones for a chain walk.
    let mut batch = Batch::new();
    for i in (1..=n).rev() {
        batch
            .dir(DirKey(i), DirKey(i + 1), b"w", &sized(4096))
            .unwrap();
    }
    let r = agg.apply(&batch);
    assert_eq!((r.parked, r.rejected), (n, 0));
    let mut batch = Batch::new();
    // A second chain, fed the other way round.
    for i in 0..n {
        batch
            .dir(DirKey(n + 10 + i), DirKey(n + 11 + i), b"w", &sized(4096))
            .unwrap();
    }
    let r = agg.apply(&batch);
    assert_eq!((r.parked, r.rejected), (n, 0));
    assert!(
        start.elapsed() < limit(),
        "parking took {:?}",
        start.elapsed()
    );

    // Closing a loop is still caught at the end of a long chain.
    let mut batch = Batch::new();
    batch
        .dir(DirKey(n + 1), DirKey(1), b"closes-the-loop", &sized(1))
        .unwrap();
    let r = agg.apply(&batch);
    assert_eq!(r.rejected, 1);
    assert_eq!(r.first_errors[0].1, bs_tree::ApplyError::Cycle);

    // Delivering the root and the head of the first chain attaches it all.
    let mut batch = Batch::new();
    batch
        .dir(DirKey(5_000_000), DirKey::NONE, b"/", &sized(4096))
        .unwrap();
    batch
        .dir(DirKey(n + 1), DirKey(5_000_000), b"head", &sized(4096))
        .unwrap();
    let r = agg.apply(&batch);
    assert_eq!(r.rejected, 0);
    assert_eq!(r.applied, 2 + n);
    let snap = agg.publish();
    snap.verify().unwrap();
    assert_eq!(snap.len(), 2 + n);
    assert_eq!(snap.lost_records(), u64::from(n) + 1, "second chain waits");
    assert!(start.elapsed() < limit() * 2, "took {:?}", start.elapsed());
}
