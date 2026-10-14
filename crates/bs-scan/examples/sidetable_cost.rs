//! Measures what the tree's side tables cost to copy when a snapshot is being
//! held, on a real directory tree.
//!
//! `bs-tree` keeps hard-link origins in a map shared with published snapshots.
//! The first write after a publication copies the map in full. This scans a
//! tree (leaving the root directory open), publishes, then applies a batch
//! that adds one more name for an already seen hard-linked file, timing it
//! right after a publication (the map is shared with the snapshot, so it is
//! copied) and again with no publication in between.
//!
//! ```text
//! sidetable_cost PATH
//! ```

use bs_scan::{ScanContext, ScanOptions, Scanner, Walker};
use bs_tree::{Aggregator, Batch, DirKey, FileKey, Meta, NodeFlags, NodeId, NodeKind, Record};
use std::time::Instant;

/// The batch without the root directory's end-of-listing record, so the root
/// stays open for more entries.
fn without_root_done(batch: &Batch) -> Batch {
    let mut out = Batch::new();
    for record in batch.records() {
        match record {
            Record::Entry(e) => {
                let name = batch.name(e.name).unwrap_or_default();
                if e.kind == NodeKind::Dir {
                    out.dir(e.key, e.parent, name, &e.meta).unwrap();
                } else {
                    out.file(e.kind, e.parent, name, &e.meta).unwrap();
                }
            }
            Record::ListingDone { dir } if *dir == DirKey(0) => {}
            Record::ListingDone { dir } => out.listing_done(*dir),
            Record::Unreadable { dir, failure } => out.unreadable(*dir, *failure),
        }
    }
    out
}

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: sidetable_cost PATH");
        std::process::exit(2);
    };
    let (ctx, rx) = ScanContext::channel(ScanOptions::default(), 64);
    let path = std::path::PathBuf::from(path);
    let walker = std::thread::spawn(move || Walker.scan(&path, &ctx));
    let mut agg = Aggregator::new();
    let mut a_link: Option<FileKey> = None;
    let mut applying = std::time::Duration::ZERO;
    let begun = Instant::now();
    for batch in rx.iter() {
        for record in batch.records() {
            if let Record::Entry(e) = record
                && e.meta.flags.contains(NodeFlags::HARD_LINK)
                && a_link.is_none()
            {
                a_link = e.meta.link;
            }
        }
        let batch = without_root_done(&batch);
        let t = Instant::now();
        agg.apply(&batch);
        applying += t.elapsed();
    }
    walker.join().unwrap().unwrap();
    println!(
        "scan wall {:.0} ms, of which the applier spent {:.0} ms applying batches",
        begun.elapsed().as_secs_f64() * 1000.0,
        applying.as_secs_f64() * 1000.0
    );

    let snap = agg.publish();
    let duplicates = (0..snap.len())
        .filter_map(|i| snap.node(NodeId::from_index(i)))
        .filter(|n| n.flags().contains(NodeFlags::HARD_LINK_DUPLICATE))
        .count();
    println!("nodes={} hard_link_duplicates={}", snap.len(), duplicates);
    drop(snap);
    let Some(link) = a_link else {
        println!("no hard-linked file in this tree");
        return;
    };

    let one_more_name = |tag: u32| {
        let mut b = Batch::new();
        b.file(
            NodeKind::File,
            DirKey(0),
            format!("extra-{tag}").as_bytes(),
            &Meta {
                link: Some(link),
                ..Meta::default()
            },
        )
        .unwrap();
        b
    };

    let mut after_publish = Vec::new();
    let mut owned = Vec::new();
    for i in 0..15u32 {
        // The published snapshot shares the map, so the first write copies it.
        agg.publish();
        let batch = one_more_name(i * 2);
        let t = Instant::now();
        let r = agg.apply(&batch);
        after_publish.push(t.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(r.rejected, 0, "{:?}", r.first_errors);
        // No publication since: the map is the aggregator's own.
        let batch = one_more_name(i * 2 + 1);
        let t = Instant::now();
        agg.apply(&batch);
        owned.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    after_publish.sort_by(f64::total_cmp);
    owned.sort_by(f64::total_cmp);
    println!(
        "first apply after a publication (copies the hard-link map): median {:.3} ms (min {:.3}, max {:.3}, n=15)",
        after_publish[7], after_publish[0], after_publish[14]
    );
    println!(
        "next apply, no publication in between:                      median {:.3} ms (min {:.3}, max {:.3}, n=15)",
        owned[7], owned[0], owned[14]
    );
    let t = Instant::now();
    let held = agg.publish();
    println!(
        "publish: {:.3} ms (epoch {})",
        t.elapsed().as_secs_f64() * 1000.0,
        held.epoch()
    );
}
