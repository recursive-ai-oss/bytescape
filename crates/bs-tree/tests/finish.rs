//! What `finish` does: stops the scan, frees the scan-time bookkeeping, and
//! leaves delete marks and publication working.

use bs_tree::{
    Aggregator, ApplyError, Batch, DeleteState, DirKey, FileKey, Meta, NodeId, NodeKind,
};

fn sized(allocated: u64) -> Meta {
    Meta {
        logical: allocated,
        allocated,
        ..Meta::default()
    }
}

fn scanned() -> Aggregator {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"r", &sized(4096)).unwrap();
    b.dir(DirKey(1), DirKey(0), b"d", &sized(4096)).unwrap();
    for i in 0..10u128 {
        let m = Meta {
            link: Some(FileKey { volume: 1, id: i }),
            ..sized(4096)
        };
        b.file(NodeKind::File, DirKey(1), format!("f{i}").as_bytes(), &m)
            .unwrap();
    }
    b.listing_done(DirKey(1));
    b.listing_done(DirKey(0));
    assert_eq!(agg.apply(&b).rejected, 0);
    agg
}

#[test]
fn after_finish_marks_undo_and_publication_still_work() {
    let mut agg = scanned();
    let report = agg.finish();
    assert_eq!(report.unresolved, 0);
    assert!(agg.is_finished());
    assert_eq!(agg.finish().unresolved, 0, "finish is idempotent");

    let snap = agg.publish();
    assert!(snap.scan_complete());
    let d = NodeId::from_index(1);
    let moved = agg.mark_deleted(d, DeleteState::InBin).unwrap();
    assert_eq!(moved.bytes.allocated, 11 * 4096);
    let snap = agg.publish();
    assert_eq!(snap.root().unwrap().current_allocated(), 4096);
    agg.unmark_deleted(d).unwrap();
    let snap = agg.publish();
    assert_eq!(snap.root().unwrap().current_allocated(), 12 * 4096);
    snap.verify().unwrap();
}

#[test]
fn nothing_is_accepted_after_finish() {
    let mut agg = scanned();
    agg.finish();
    let mut b = Batch::new();
    b.file(NodeKind::File, DirKey(1), b"late", &sized(1))
        .unwrap();
    b.dir(DirKey(7), DirKey(0), b"late-dir", &sized(1)).unwrap();
    b.listing_done(DirKey(0));
    let r = agg.apply(&b);
    assert_eq!((r.applied, r.parked, r.rejected), (0, 0, 3));
    assert!(
        r.first_errors
            .iter()
            .all(|e| e.1 == ApplyError::ScanFinished)
    );
    let snap = agg.publish();
    assert_eq!(snap.len(), 12, "nothing was added");
    assert_eq!(snap.lost_records(), 3);
    assert!(!snap.scan_complete());
    assert!(snap.root().unwrap().bytes_are_lower_bound());
}

#[test]
fn a_cancelled_scan_stays_incomplete_after_finish() {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"r", &sized(4096)).unwrap();
    b.dir(DirKey(1), DirKey(0), b"open", &sized(4096)).unwrap();
    agg.apply(&b);
    agg.mark_stopped();
    agg.finish();
    let snap = agg.publish();
    assert!(snap.scan_stopped());
    assert!(!snap.scan_complete());
    assert!(snap.root().unwrap().is_in_progress());
    assert!(snap.node(NodeId::from_index(1)).unwrap().is_in_progress());
    assert!(snap.root().unwrap().bytes_are_lower_bound());
}
