//! Snapshots read from other threads while the owner applies batches.

mod common;

use bs_tree::{Aggregator, NodeId, Snapshot};
use common::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

/// Everything observable in a snapshot, for "it did not change" checks.
fn fingerprint(snap: &Snapshot) -> Vec<String> {
    (0..snap.len())
        .map(|i| {
            let n = snap.node(NodeId::from_index(i)).unwrap();
            format!(
                "{:?} {} {} {} {} {:?}",
                n.name(),
                n.logical(),
                n.allocated(),
                n.file_count(),
                n.is_complete(),
                n.parent().map(|p| p.id().index()),
            )
        })
        .collect()
}

#[test]
fn readers_see_consistent_monotone_snapshots_while_batches_are_applied() {
    let raw: Vec<(u32, bool, u16, u16)> = (0..20000u32)
        .map(|i| {
            (
                i.wrapping_mul(2654435761) >> 9,
                i % 6 == 0,
                (i % 1000) as u16,
                (i % 11) as u16,
            )
        })
        .collect();
    let spec = spec_from_raw(&raw);
    let expected = expected_totals(&spec)[0];
    let mut batches = batches_for(&spec);
    // Interleave arrival so children often come before their parents.
    Rng(0xC0FFEE).shuffle(&mut batches);

    let mut agg = Aggregator::new();
    let reader = agg.reader();
    let done = Arc::new(AtomicBool::new(false));

    let readers: Vec<_> = (0..4)
        .map(|t| {
            let reader = reader.clone();
            let done = Arc::clone(&done);
            thread::spawn(move || {
                let mut last_epoch = 0;
                let mut last_root_alloc = 0;
                let mut seen = 0usize;
                let mut epochs_seen = 0usize;
                let mut pinned: Option<(Arc<Snapshot>, Vec<_>)> = None;
                loop {
                    let finished = done.load(Ordering::Acquire);
                    let snap = reader.load();
                    assert!(snap.epoch() >= last_epoch, "epochs never go backwards");
                    if snap.epoch() > last_epoch || finished {
                        last_epoch = snap.epoch();
                        epochs_seen += 1;
                        // Each distinct snapshot must satisfy every invariant.
                        // Verifying all of them would starve the writer, so
                        // threads take turns.
                        if (snap.epoch() as usize + t).is_multiple_of(4) || finished {
                            snap.verify()
                                .unwrap_or_else(|v| panic!("epoch {}: {v}", snap.epoch()));
                            seen += 1;
                        }
                        if let Some(root) = snap.root() {
                            assert!(
                                root.allocated() >= last_root_alloc,
                                "later snapshots never hold fewer bytes"
                            );
                            last_root_alloc = root.allocated();
                        }
                        if pinned.is_none() && snap.len() > 100 {
                            pinned = Some((Arc::clone(&snap), fingerprint(&snap)));
                        }
                    }
                    if finished {
                        break;
                    }
                }
                // A snapshot held across the whole run never changed.
                if let Some((snap, before)) = pinned {
                    assert_eq!(fingerprint(&snap), before);
                }
                (seen, epochs_seen)
            })
        })
        .collect();

    for (i, batch) in batches.iter().enumerate() {
        assert_eq!(agg.apply(batch).rejected, 0);
        if i % 3 == 0 {
            agg.publish();
        }
    }
    assert_eq!(agg.finish().unresolved, 0);
    let last = agg.publish();
    done.store(true, Ordering::Release);

    let results: Vec<(usize, usize)> = readers.into_iter().map(|h| h.join().unwrap()).collect();
    let verified: usize = results.iter().map(|r| r.0).sum();
    let most_epochs = results.iter().map(|r| r.1).max().unwrap_or(0);
    assert!(verified > 0, "readers must have checked snapshots");
    assert!(
        most_epochs > 2,
        "readers must have seen the tree change underneath them"
    );
    assert!(last.scan_complete());
    let root = last.root().unwrap();
    assert_eq!(
        (
            root.logical(),
            root.allocated(),
            u64::from(root.file_count())
        ),
        expected
    );
    assert_eq!(reader.load().epoch(), last.epoch());
}

#[test]
fn root_totals_never_decrease_across_published_snapshots() {
    let raw: Vec<(u32, bool, u16, u16)> = (0..2000u32)
        .map(|i| {
            (
                i.wrapping_mul(40503) >> 4,
                i % 5 == 0,
                (i % 300) as u16,
                (i % 5) as u16,
            )
        })
        .collect();
    let spec = spec_from_raw(&raw);
    let mut agg = Aggregator::new();
    let mut last = (0, 0, 0);
    let mut incomplete_seen = false;
    for batch in batches_for(&spec) {
        agg.apply(&batch);
        let snap = agg.publish();
        if let Some(root) = snap.root() {
            let now = (root.logical(), root.allocated(), root.file_count());
            assert!(now.0 >= last.0 && now.1 >= last.1 && now.2 >= last.2);
            last = now;
            incomplete_seen |= root.is_in_progress();
        }
    }
    assert!(incomplete_seen);
    assert!(agg.publish().scan_complete());
}

#[test]
fn snapshots_share_unchanged_data_and_survive_the_aggregator() {
    let spec = spec_from_raw(
        &(0..500u32)
            .map(|i| (i, i % 3 == 0, 10, 1))
            .collect::<Vec<_>>(),
    );
    let mut agg = Aggregator::new();
    for b in batches_for(&spec) {
        agg.apply(&b);
    }
    let reader = agg.reader();
    let published = agg.publish();
    let f = fingerprint(&published);
    drop(agg);
    // The reader handle and the snapshot outlive the aggregator.
    let loaded = reader.load();
    assert_eq!(fingerprint(&loaded), f);
    assert_eq!(loaded.epoch(), published.epoch());
}

#[test]
fn publish_if_due_throttles() {
    use std::time::Duration;
    let mut agg = Aggregator::new();
    assert!(
        agg.publish_if_due(Duration::from_secs(3600)),
        "first call publishes"
    );
    assert!(!agg.publish_if_due(Duration::from_secs(3600)));
    assert_eq!(agg.reader().load().epoch(), 1);
    assert!(agg.publish_if_due(Duration::ZERO));
    assert_eq!(agg.reader().load().epoch(), 2);
}
