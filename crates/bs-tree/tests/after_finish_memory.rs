//! After `finish`, an aggregator that is kept for delete marks must hold
//! little more than the tree. This binary holds a single test and a counting
//! allocator so that no other thread's allocations are in the figures.

use bs_tree::{Aggregator, Batch, DeleteState, DirKey, FileKey, Meta, NodeId, NodeKind};
use stats_alloc::{INSTRUMENTED_SYSTEM, StatsAlloc};
use std::alloc::System;

#[global_allocator]
static GLOBAL: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

fn live_bytes() -> i64 {
    let s = INSTRUMENTED_SYSTEM.stats();
    s.bytes_allocated as i64 - s.bytes_deallocated as i64
}

#[test]
fn finish_frees_the_scan_time_bookkeeping_even_when_every_file_is_keyed() {
    const FILES_PER_DIR: u32 = 20;
    const DIRS: u32 = 5_000;
    let nodes = i64::from(1 + DIRS * (1 + FILES_PER_DIR));

    let before = live_bytes();
    let mut agg = Aggregator::new();
    let mut batch = Batch::new();
    batch
        .dir(DirKey(0), DirKey::NONE, b"", &Meta::default())
        .unwrap();
    let mut file_id = 0u128;
    for d in 1..=DIRS {
        batch
            .dir(DirKey(d), DirKey(0), b"", &Meta::default())
            .unwrap();
        for _ in 0..FILES_PER_DIR {
            file_id += 1;
            let meta = Meta {
                allocated: 4096,
                logical: 100,
                link: Some(FileKey {
                    volume: 1,
                    id: file_id,
                }),
                ..Meta::default()
            };
            batch.file(NodeKind::File, DirKey(d), b"", &meta).unwrap();
        }
        batch.listing_done(DirKey(d));
        if batch.len() > 4000 {
            assert_eq!(agg.apply(&batch).rejected, 0);
            batch.clear();
        }
    }
    batch.listing_done(DirKey(0));
    assert_eq!(agg.apply(&batch).rejected, 0);
    drop(batch);

    let scanning = live_bytes() - before;
    assert_eq!(agg.finish().unresolved, 0);
    let finished = live_bytes() - before;

    let per_node = finished as f64 / nodes as f64;
    let per_node_scanning = scanning as f64 / nodes as f64;
    // The columns take 47 B/node and each empty name one byte of pool, so a
    // tree on its own is a little over 48; the bookkeeping used to add more
    // than 100 more with every file keyed.
    assert!(
        per_node_scanning > 100.0,
        "test premise: {per_node_scanning:.1} B/node while scanning"
    );
    assert!(
        per_node < 52.0,
        "{per_node:.1} B/node held after finish (was {per_node_scanning:.1} while scanning)"
    );
    // The tree that is left is whole, and delete marks and their undo work.
    let snap = agg.publish();
    assert!(snap.scan_complete());
    let root_allocated = snap.root().unwrap().allocated();
    drop(snap);
    let moved = agg
        .mark_deleted(NodeId::ROOT, DeleteState::InBin)
        .expect("a finished scan's root can be marked");
    assert_eq!(moved.bytes.allocated, root_allocated);
    assert!(!moved.lower_bound);
    assert_eq!(agg.publish().root().unwrap().current_allocated(), 0);
    agg.unmark_deleted(NodeId::ROOT).unwrap();
    assert_eq!(
        agg.publish().root().unwrap().current_allocated(),
        root_allocated
    );
    drop(agg);
    assert!(live_bytes() - before < 65_536, "everything is released");
}
