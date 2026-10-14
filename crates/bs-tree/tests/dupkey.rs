//! Two waiting directories with the same key must not let a later flood of
//! waiting directories turn quadratic.

use bs_tree::{Aggregator, ApplyError, Batch, DirKey, Meta};
use std::time::{Duration, Instant};

fn dir(b: &mut Batch, key: u32, parent: u32) {
    b.dir(DirKey(key), DirKey(parent), b"d", &Meta::default())
        .unwrap();
}

/// Generous in debug builds, tight in release builds.
fn limit() -> Duration {
    if cfg!(debug_assertions) {
        Duration::from_secs(20)
    } else {
        Duration::from_secs(1)
    }
}

const N: u32 = 100_000;

fn root(agg: &mut Aggregator) {
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"r", &Meta::default())
        .unwrap();
    assert_eq!(agg.apply(&b).rejected, 0);
}

/// Waiting chain 2 -> 3 -> 1 -> 100, where 100 has not arrived. Fed
/// bottom-up, which is what lets a later duplicate of key 1 be placed under 2.
fn waiting_chain(b: &mut Batch) {
    dir(b, 2, 3);
    dir(b, 3, 1);
    dir(b, 1, 100);
}

#[test]
fn a_duplicate_of_a_waiting_directory_key_is_refused() {
    let mut agg = Aggregator::new();
    root(&mut agg);
    let mut b = Batch::new();
    waiting_chain(&mut b);
    assert_eq!(agg.apply(&b).rejected, 0);

    // Key 1 again, this time under 2: the first one's link would be replaced
    // by one that closes a loop.
    let mut b = Batch::new();
    dir(&mut b, 1, 2);
    let r = agg.apply(&b);
    assert_eq!((r.parked, r.rejected), (0, 1));
    assert_eq!(r.first_errors[0].1, ApplyError::DuplicateKey(DirKey(1)));

    // The loss is not silent: its place is unknown, so the root is flagged.
    let snap = agg.publish();
    assert!(snap.root().unwrap().has_lost_records_below());
    assert!(snap.root().unwrap().bytes_are_lower_bound());
    assert_eq!(snap.lost_records(), 4, "three waiting, one refused");
    snap.verify().unwrap();
}

#[test]
fn a_flood_of_waiting_directories_after_a_duplicate_key_stays_linear() {
    let mut agg = Aggregator::new();
    root(&mut agg);
    let mut b = Batch::new();
    waiting_chain(&mut b);
    dir(&mut b, 1, 2); // the duplicate
    let r = agg.apply(&b);
    // Checked before anything is timed: without the refusal the flood below
    // takes minutes.
    assert_eq!(r.rejected, 1, "the duplicate key must be refused");
    assert_eq!(r.first_errors[0].1, ApplyError::DuplicateKey(DirKey(1)));

    let mut b = Batch::new();
    for i in 0..N {
        dir(&mut b, 1000 + i, 2);
    }
    let start = Instant::now();
    let r = agg.apply(&b);
    let took = start.elapsed();
    assert_eq!((r.parked, r.rejected), (N, 0));
    assert!(took < limit(), "parking {N} directories took {took:?}");

    // When the missing directory arrives, everything attaches: the chain
    // 100 > 1 > 3 > 2 and the flood under 2.
    let mut b = Batch::new();
    dir(&mut b, 100, 0);
    let start = Instant::now();
    let r = agg.apply(&b);
    assert_eq!((r.applied, r.rejected), (4 + N, 0));
    assert!(
        start.elapsed() < limit(),
        "attaching took {:?}",
        start.elapsed()
    );
    let snap = agg.publish();
    snap.verify().unwrap();
    assert_eq!(snap.len(), 5 + N);
}

/// The duplicate's own parent is already known, so it never reaches the
/// waiting table: the later record must still be the one refused, whatever
/// order the parents arrive in.
#[test]
fn a_duplicate_of_a_waiting_key_with_a_known_parent_is_refused() {
    use bs_tree::NodeKind;
    let sized = |n: u64| Meta {
        logical: n,
        allocated: n,
        ..Meta::default()
    };
    let mut agg = Aggregator::new();
    root(&mut agg);

    // The first directory with key 1 waits for key 100, and has a child.
    let mut b = Batch::new();
    b.dir(DirKey(1), DirKey(100), b"first", &Meta::default())
        .unwrap();
    b.file(NodeKind::File, DirKey(1), b"x", &sized(4096))
        .unwrap();
    let r = agg.apply(&b);
    assert_eq!((r.parked, r.rejected), (2, 0));

    // A second directory with key 1 whose parent (the root) is known.
    let mut b = Batch::new();
    b.dir(DirKey(1), DirKey(0), b"second", &Meta::default())
        .unwrap();
    let r = agg.apply(&b);
    assert_eq!((r.applied, r.parked, r.rejected), (0, 0, 1));
    assert_eq!(r.first_errors[0].1, ApplyError::DuplicateKey(DirKey(1)));
    // Its place is known (the root), so the root is flagged.
    let snap = agg.publish();
    assert!(snap.root().unwrap().has_lost_records_below());
    assert_eq!(snap.len(), 1, "nothing was added");

    // The missing parent arrives: the first directory attaches with its
    // child, under the name it was sent with.
    let mut b = Batch::new();
    b.dir(DirKey(100), DirKey(0), b"hundred", &Meta::default())
        .unwrap();
    b.listing_done(DirKey(1));
    b.listing_done(DirKey(100));
    b.listing_done(DirKey(0));
    let r = agg.apply(&b);
    assert_eq!(r.rejected, 0, "{:?}", r.first_errors);
    let snap = agg.publish();
    snap.verify().unwrap();
    assert_eq!(snap.len(), 4);
    let first = snap
        .root()
        .unwrap()
        .children()
        .find(|c| c.name() == b"hundred")
        .and_then(|h| h.children().find(|c| c.name() == b"first"))
        .expect("the first directory is under 100");
    assert_eq!(first.allocated(), 4096);
    assert_eq!(first.file_count(), 1);
    assert!(
        snap.root()
            .unwrap()
            .children()
            .all(|c| c.name() != b"second")
    );
}
