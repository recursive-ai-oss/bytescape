//! Rescanning one folder of a tree built by the Linux walker: the result is
//! the tree a fresh scan of the same files would give, the folder keeps its
//! place, and a scan that cannot be trusted changes nothing.
#![cfg(target_os = "linux")]

mod support;

use bs_scan::{
    ScanContext, ScanEnd, ScanError, ScanOptions, ScanResult, Scanner, Walker, node_path,
    start_rescan,
};
use bs_tree::{
    Aggregator, Batch, DeleteState, DirKey, Meta, NodeId, NodeKind, ReadErrorKind, RescanError,
    Snapshot,
};
use std::collections::BTreeMap;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use support::*;

type View = BTreeMap<String, (NodeKind, u64, u64, u32, bool)>;

/// Per path below the root: kind, sizes, file count, complete.
fn view(snap: &Snapshot) -> View {
    let mut out = View::new();
    let root_name = String::from_utf8_lossy(snap.root().unwrap().name()).into_owned();
    let mut stack = vec![snap.root().unwrap().id()];
    while let Some(id) = stack.pop() {
        let n = snap.node(id).unwrap();
        let full = String::from_utf8_lossy(&snap.path(id, b'/').unwrap()).into_owned();
        let rel = full
            .strip_prefix(&root_name)
            .unwrap_or_default()
            .trim_start_matches('/')
            .to_owned();
        out.insert(
            rel,
            (
                n.kind(),
                n.logical(),
                n.allocated(),
                n.file_count(),
                n.is_complete(),
            ),
        );
        stack.extend(n.children().map(|c| c.id()));
    }
    out
}

fn id_of(snap: &Snapshot, rel: &str) -> NodeId {
    node_at(snap, rel).id()
}

fn rescan(result: ScanResult, rel: &str) -> ScanResult {
    let node = id_of(&result.snapshot, rel);
    let path = node_path(&result.snapshot, node).unwrap();
    let handle = start_rescan(
        result.aggregator,
        node,
        Arc::new(Walker),
        path,
        ScanOptions::default(),
    )
    .map_err(|r| r.error)
    .unwrap();
    handle.wait()
}

fn assert_rescan_clean(result: &ScanResult) {
    assert!(
        result.stats.error.is_none(),
        "rescan failed: {:?}",
        result.stats.error
    );
    assert_eq!(result.stats.end, Some(ScanEnd::Completed));
    assert_eq!(result.stats.records_parked, 0);
    assert_eq!(result.stats.records_rejected, 0);
    result.snapshot.verify().expect("tree invariants");
    assert!(result.snapshot.scan_complete());
}

fn sample(fx: &Fixture) {
    fx.file("a/one.txt", 100);
    fx.file("a/sub/two.bin", 70_000);
    fx.file("a/sub/deep/three.bin", 9000);
    fx.dir("a/empty");
    fx.file("b/four.txt", 5000);
    fx.file("top.dat", 20_000);
    fx.settle();
}

#[test]
fn a_rescan_gives_what_a_fresh_scan_of_the_same_files_gives() {
    let fx = Fixture::new("rescan-equal");
    sample(&fx);
    let first = scan(&fx.root);
    assert_clean(&first);
    let a = id_of(&first.snapshot, "a");
    let b = id_of(&first.snapshot, "b");
    let old_sub = id_of(&first.snapshot, "a/sub");
    let before_epoch = first.snapshot.epoch();

    // Files appear and disappear inside `a`.
    std::fs::remove_file(fx.path("a/one.txt")).unwrap();
    std::fs::remove_dir_all(fx.path("a/sub/deep")).unwrap();
    fx.file("a/new/big.bin", 300_000);
    fx.file("a/sub/two.bin", 12_000);
    fx.settle();

    let second = rescan(first, "a");
    assert_rescan_clean(&second);
    let fresh = scan(&fx.root);
    assert_clean(&fresh);
    assert_eq!(view(&second.snapshot), view(&fresh.snapshot));

    // The folder, its sibling and its parent are where they were.
    let snap = &second.snapshot;
    assert!(snap.epoch() > before_epoch);
    assert_eq!(id_of(snap, "a"), a);
    assert_eq!(id_of(snap, "b"), b);
    assert_eq!(node_at(snap, "a").parent().unwrap().id(), NodeId::ROOT);
    // What was inside is cut off; the new `sub` is another node.
    assert!(snap.node(old_sub).unwrap().is_detached());
    assert_ne!(id_of(snap, "a/sub"), old_sub);
    assert_eq!(
        snap.root().unwrap().name(),
        fx.root.as_os_str().as_encoded_bytes()
    );
    // Same numbers as GNU du for the whole tree.
    assert_matches_du(&fx.root, &second);
}

#[test]
fn the_whole_tree_can_be_rescanned_from_its_root() {
    let fx = Fixture::new("rescan-root");
    sample(&fx);
    let first = scan(&fx.root);
    fx.file("added.bin", 40_000);
    fx.settle();
    let node = NodeId::ROOT;
    let path = node_path(&first.snapshot, node).unwrap();
    assert_eq!(path, fx.root);
    let second = start_rescan(
        first.aggregator,
        node,
        Arc::new(Walker),
        path,
        ScanOptions::default(),
    )
    .map_err(|r| r.error)
    .unwrap()
    .wait();
    assert_rescan_clean(&second);
    assert_eq!(view(&second.snapshot), view(&scan(&fx.root).snapshot));
}

#[test]
fn delete_marks_elsewhere_survive_and_marks_inside_are_dropped_with_the_contents() {
    let fx = Fixture::new("rescan-marks");
    sample(&fx);
    let mut first = scan(&fx.root);
    let four = id_of(&first.snapshot, "b/four.txt");
    let two = id_of(&first.snapshot, "a/sub/two.bin");
    first
        .aggregator
        .mark_deleted(four, DeleteState::InBin)
        .unwrap();
    first
        .aggregator
        .mark_deleted(two, DeleteState::Deleted)
        .unwrap();
    let moved_outside = first
        .aggregator
        .publish()
        .node(four)
        .unwrap()
        .delete_totals();

    // The file marked inside is really gone from disk; the other is in the
    // bin (still in `b` on this fixture, to keep the sizes simple).
    std::fs::remove_file(fx.path("a/sub/two.bin")).unwrap();
    fx.settle();
    let mut second = rescan(first, "a");
    assert_rescan_clean(&second);
    let snap = second.aggregator.publish();
    snap.verify().unwrap();
    assert_eq!(snap.node(four).unwrap().delete_state(), DeleteState::InBin);
    let root_totals = snap.root().unwrap().delete_totals();
    assert_eq!(root_totals, moved_outside);
    assert_eq!(root_totals.deleted.allocated, 0);
    assert_eq!(second.aggregator.rescan_report().unwrap().marks_dropped, 1);
    // Undo still works for the one outside.
    second.aggregator.unmark_deleted(four).unwrap();
    let snap = second.aggregator.publish();
    assert_eq!(snap.root().unwrap().delete_totals(), Default::default());
    snap.verify().unwrap();
}

#[test]
fn a_hard_link_between_the_folder_and_the_rest_is_counted_once_before_and_after() {
    let fx = Fixture::new("rescan-links");
    fx.file("a/original.bin", 50_000);
    fx.file("b/other.txt", 10);
    std::fs::hard_link(fx.path("a/original.bin"), fx.path("b/link.bin")).unwrap();
    fx.settle();
    let first = scan(&fx.root);
    assert_clean(&first);
    let bytes = first.snapshot.root().unwrap().allocated();

    // `a` loses its name for the file; `b` still has one.
    std::fs::remove_file(fx.path("a/original.bin")).unwrap();
    fx.settle();
    let second = rescan(first, "a");
    assert_rescan_clean(&second);
    let snap = &second.snapshot;
    let link = node_at(snap, "b/link.bin");
    assert!(link.allocated() >= 50_000);
    assert!(link.is_hard_link());
    // Exactly what a fresh scan counts now (the file is counted once).
    let fresh = scan(&fx.root);
    assert_eq!(
        (
            snap.root().unwrap().logical(),
            snap.root().unwrap().allocated()
        ),
        (
            fresh.snapshot.root().unwrap().logical(),
            fresh.snapshot.root().unwrap().allocated()
        )
    );
    assert!(snap.root().unwrap().allocated() <= bytes);

    // And the other way round: the folder gets a name for it again.
    std::fs::hard_link(fx.path("b/link.bin"), fx.path("a/again.bin")).unwrap();
    fx.settle();
    let third = rescan(second, "a");
    assert_rescan_clean(&third);
    // The totals are those of a fresh scan. Which name carries the bytes
    // can differ (a scan counts the first name it meets), so the names are
    // compared by what they add up to.
    let fresh = scan(&fx.root);
    let totals = |s: &Snapshot| {
        let r = s.root().unwrap();
        (r.logical(), r.allocated(), r.file_count())
    };
    assert_eq!(totals(&third.snapshot), totals(&fresh.snapshot));
    let again = node_at(&third.snapshot, "a/again.bin");
    let link = node_at(&third.snapshot, "b/link.bin");
    assert!(again.is_hard_link() && link.is_hard_link());
    assert_eq!(
        again.allocated() + link.allocated(),
        node_at(&fresh.snapshot, "a/again.bin").allocated()
            + node_at(&fresh.snapshot, "b/link.bin").allocated()
    );
    assert!(
        (again.allocated() == 0) != (link.allocated() == 0),
        "counted at one name only"
    );
}

#[test]
fn an_unreadable_folder_inside_is_recorded_and_the_flag_clears_when_it_is_readable_again() {
    if is_root() {
        return;
    }
    let fx = Fixture::new("rescan-unreadable");
    sample(&fx);
    fx.file("a/locked/inner.txt", 100);
    fx.settle();
    std::fs::set_permissions(fx.path("a/locked"), std::fs::Permissions::from_mode(0o000)).unwrap();
    let first = scan(&fx.root);
    assert!(first.snapshot.root().unwrap().has_unreadable_below());
    std::fs::set_permissions(fx.path("a/locked"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let second = rescan(first, "a");
    assert_rescan_clean(&second);
    let snap = &second.snapshot;
    assert!(!snap.root().unwrap().has_unreadable_below());
    assert!(!snap.root().unwrap().bytes_are_lower_bound());
    assert_eq!(node_at(snap, "a/locked/inner.txt").logical(), 100);

    // Made unreadable again: recorded, not fatal, and the ancestors say so.
    std::fs::set_permissions(fx.path("a/locked"), std::fs::Permissions::from_mode(0o000)).unwrap();
    let third = rescan(second, "a");
    assert_rescan_clean(&third);
    let locked = node_at(&third.snapshot, "a/locked");
    assert!(locked.is_unreadable());
    assert_eq!(
        locked.read_failure().unwrap().kind,
        ReadErrorKind::PermissionDenied
    );
    assert!(third.snapshot.root().unwrap().has_unreadable_below());
    assert!(third.snapshot.root().unwrap().bytes_are_lower_bound());
}

// ---- what a scan that cannot open the folder does -----------------------------------------

#[test]
fn a_folder_that_is_gone_or_cannot_be_opened_leaves_the_tree_as_it_was() {
    let fx = Fixture::new("rescan-gone");
    sample(&fx);
    let first = scan(&fx.root);
    let before = view(&first.snapshot);
    std::fs::remove_dir_all(fx.path("a")).unwrap();
    let second = rescan(first, "a");
    match &second.stats.error {
        Some(ScanError::Root(e)) => assert_eq!(e.kind(), std::io::ErrorKind::NotFound),
        other => panic!("expected a root error, got {other:?}"),
    }
    // Nothing was replaced, and the report says so.
    let report = second.aggregator.rescan_report().unwrap();
    assert!(!report.replaced && !report.refused);
    second.snapshot.verify().unwrap();
    assert_eq!(view(&second.snapshot), before);
    assert!(second.snapshot.scan_complete());
    assert!(!second.snapshot.scan_stopped());
    assert_eq!(second.snapshot.detached_nodes(), 0);
    // The aggregator is back to normal and can rescan something else.
    let third = rescan(second, "b");
    assert_rescan_clean(&third);
}

#[test]
fn a_path_that_became_a_link_is_refused_without_reading_through_it() {
    let fx = Fixture::new("rescan-swapped");
    fx.file("a/sub/real.txt", 1000);
    fx.file("elsewhere/sub/other.txt", 777_000);
    fx.settle();
    let first = scan(&fx.root);
    let before = view(&first.snapshot);
    // `a` is replaced by a link to somewhere else with a folder of the same
    // name inside.
    std::fs::rename(fx.path("a"), fx.path("a-moved")).unwrap();
    symlink(fx.path("elsewhere"), fx.path("a")).unwrap();
    let node = id_of(&first.snapshot, "a/sub");
    let path = node_path(&first.snapshot, node).unwrap();
    assert_eq!(path, fx.path("a/sub"));
    let second = start_rescan(
        first.aggregator,
        node,
        Arc::new(Walker),
        path,
        ScanOptions::default(),
    )
    .map_err(|r| r.error)
    .unwrap()
    .wait();
    let report = second.aggregator.rescan_report().unwrap();
    assert!(report.refused && !report.replaced, "{report:?}");
    second.snapshot.verify().unwrap();
    assert_eq!(view(&second.snapshot), before);
    assert_eq!(second.snapshot.lost_records(), 0);
    assert!(second.snapshot.scan_complete());
    // Nothing from the other folder is in the tree.
    assert!(try_node_at(&second.snapshot, "a/sub/other.txt").is_none());
    assert!(!second.snapshot.scan_stopped());
}

#[test]
fn start_rescan_gives_the_aggregator_back_when_it_cannot_begin() {
    let fx = Fixture::new("rescan-refuse");
    sample(&fx);
    let first = scan(&fx.root);
    let file = id_of(&first.snapshot, "top.dat");
    let Err(refused) = start_rescan(
        first.aggregator,
        file,
        Arc::new(Walker),
        fx.path("top.dat"),
        ScanOptions::default(),
    ) else {
        panic!("a file cannot be rescanned")
    };
    assert_eq!(refused.error, RescanError::NotADirectory(file));
    assert!(!refused.to_string().is_empty());
    // The tree is still the one it was.
    let mut agg = refused.aggregator;
    assert!(agg.publish().scan_complete());

    // A tree whose own scan has not finished.
    let Err(refused) = start_rescan(
        Aggregator::new(),
        NodeId::ROOT,
        Arc::new(Walker),
        fx.root.clone(),
        ScanOptions::default(),
    ) else {
        panic!("no tree yet")
    };
    assert_eq!(refused.error, RescanError::ScanRunning);

    // A folder that is in the bin.
    let a = id_of(&agg.publish(), "a");
    agg.mark_deleted(a, DeleteState::InBin).unwrap();
    let Err(refused) = start_rescan(
        agg,
        a,
        Arc::new(Walker),
        fx.path("a"),
        ScanOptions::default(),
    ) else {
        panic!("a deleted folder cannot be rescanned")
    };
    assert_eq!(refused.error, RescanError::Marked(a));
}

#[test]
fn node_path_names_real_folders_and_not_cut_off_ones() {
    let fx = Fixture::new("rescan-paths");
    sample(&fx);
    fx.file("odd \u{e9}\n name/f.txt", 1);
    let first = scan(&fx.root);
    let snap = &first.snapshot;
    assert_eq!(node_path(snap, NodeId::ROOT).unwrap(), fx.root);
    assert_eq!(
        node_path(snap, id_of(snap, "a/sub/deep")).unwrap(),
        fx.path("a/sub/deep")
    );
    assert_eq!(
        node_path(snap, id_of(snap, "odd \u{e9}\n name")).unwrap(),
        fx.path("odd \u{e9}\n name")
    );
    assert_eq!(node_path(snap, NodeId::from_index(99_999)), None);
    let old = id_of(snap, "a/sub");
    let second = rescan(first, "a");
    assert_eq!(node_path(&second.snapshot, old), None);
}

// ---- scans that stop part way -------------------------------------------------------------------

/// A scanner that sends the folder, one subfolder, and then does what the
/// test says.
struct Scripted {
    /// The name the root record carries.
    root_name: Vec<u8>,
    ending: Ending,
    /// Told when the first batch has been sent; the scan then waits for
    /// `go`.
    sent: Option<mpsc::SyncSender<()>>,
    go: std::sync::Mutex<Option<mpsc::Receiver<()>>>,
}

enum Ending {
    /// Everything listed.
    Complete,
    /// An error after the folder was sent.
    Fail(std::io::ErrorKind),
    /// Cancelled after the folder was sent.
    Cancel,
    /// Keeps sending until cancelled (for the refused scan).
    Spin,
}

impl Scanner for Scripted {
    fn name(&self) -> &'static str {
        "scripted"
    }

    fn scan(&self, _root: &Path, ctx: &ScanContext) -> Result<ScanEnd, ScanError> {
        let root = ctx.next_dir_key().unwrap();
        let sub = ctx.next_dir_key().unwrap();
        let mut batch = Batch::new();
        batch
            .dir(root, DirKey::NONE, &self.root_name, &Meta::default())
            .unwrap();
        batch.dir(sub, root, b"half", &Meta::default()).unwrap();
        batch
            .file(
                NodeKind::File,
                sub,
                b"f",
                &Meta {
                    logical: 5,
                    allocated: 4096,
                    ..Meta::default()
                },
            )
            .unwrap();
        ctx.send(batch);
        if let Some(sent) = &self.sent {
            sent.send(()).unwrap();
        }
        if let Some(go) = self.go.lock().unwrap().take() {
            go.recv_timeout(Duration::from_secs(30)).unwrap();
        }
        match &self.ending {
            Ending::Complete => {
                let mut rest = Batch::new();
                rest.listing_done(sub);
                rest.listing_done(root);
                ctx.send(rest);
                Ok(ScanEnd::Completed)
            }
            Ending::Fail(kind) => Err(ScanError::Root(std::io::Error::from(*kind))),
            Ending::Cancel => Ok(ScanEnd::Cancelled),
            Ending::Spin => {
                let started = Instant::now();
                while !ctx.is_cancelled() && started.elapsed() < Duration::from_secs(20) {
                    let mut more = Batch::new();
                    more.file(NodeKind::File, root, b"x", &Meta::default())
                        .unwrap();
                    if !ctx.send(more) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                Ok(ScanEnd::Cancelled)
            }
        }
    }
}

fn scripted(fx: &Fixture, ending: Ending) -> (Arc<Scripted>, ScanResult) {
    sample(fx);
    let first = scan(&fx.root);
    let name = node_path(&first.snapshot, id_of(&first.snapshot, "a"))
        .unwrap()
        .into_os_string()
        .into_encoded_bytes();
    let scanner = Arc::new(Scripted {
        root_name: name,
        ending,
        sent: None,
        go: std::sync::Mutex::new(None),
    });
    (scanner, first)
}

fn run_scripted(first: ScanResult, scanner: Arc<Scripted>) -> ScanResult {
    let a = id_of(&first.snapshot, "a");
    let path = node_path(&first.snapshot, a).unwrap();
    start_rescan(first.aggregator, a, scanner, path, ScanOptions::default())
        .map_err(|r| r.error)
        .unwrap()
        .wait()
}

#[test]
fn an_error_after_the_folder_was_read_leaves_it_unreadable_and_incomplete() {
    let fx = Fixture::new("rescan-fails");
    let (scanner, first) = scripted(&fx, Ending::Fail(std::io::ErrorKind::PermissionDenied));
    let second = run_scripted(first, scanner);
    assert!(matches!(second.stats.error, Some(ScanError::Root(_))));
    second.snapshot.verify().unwrap();
    let a = node_at(&second.snapshot, "a");
    assert!(a.is_unreadable());
    assert!(!a.is_complete());
    assert_eq!(
        a.read_failure().unwrap().kind,
        ReadErrorKind::PermissionDenied
    );
    assert!(a.bytes_are_lower_bound());
    let root = second.snapshot.root().unwrap();
    assert!(!root.is_complete() && root.bytes_are_lower_bound());
    assert!(!second.snapshot.scan_complete());
    // What was not rescanned is untouched.
    assert!(node_at(&second.snapshot, "b").is_complete());
    assert_eq!(node_at(&second.snapshot, "b/four.txt").logical(), 5000);
}

#[test]
fn a_cancelled_rescan_keeps_what_it_counted_marked_incomplete() {
    let fx = Fixture::new("rescan-cancelled");
    let (scanner, first) = scripted(&fx, Ending::Cancel);
    let second = run_scripted(first, scanner);
    assert_eq!(second.stats.end, Some(ScanEnd::Cancelled));
    second.snapshot.verify().unwrap();
    assert!(second.snapshot.scan_stopped());
    let a = node_at(&second.snapshot, "a");
    assert!(a.is_in_progress());
    assert_eq!(node_at(&second.snapshot, "a/half/f").logical(), 5);
    assert!(second.snapshot.root().unwrap().bytes_are_lower_bound());
    assert!(node_at(&second.snapshot, "b").is_complete());
}

#[test]
fn a_scan_of_the_wrong_folder_is_stopped_and_ignored() {
    let fx = Fixture::new("rescan-wrong");
    let (_, first) = scripted(&fx, Ending::Spin);
    let before = view(&first.snapshot);
    let scanner = Arc::new(Scripted {
        root_name: b"/somewhere/else".to_vec(),
        ending: Ending::Spin,
        sent: None,
        go: std::sync::Mutex::new(None),
    });
    let started = Instant::now();
    let second = run_scripted(first, scanner);
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "the applier cancels a refused scan"
    );
    let report = second.aggregator.rescan_report().unwrap();
    assert!(report.refused);
    second.snapshot.verify().unwrap();
    assert_eq!(view(&second.snapshot), before);
    assert_eq!(second.snapshot.lost_records(), 0);
    assert!(!second.snapshot.scan_stopped());
    assert!(second.snapshot.scan_complete());
}

#[test]
fn a_reader_sees_the_folder_in_progress_and_then_complete() {
    let fx = Fixture::new("rescan-progress");
    sample(&fx);
    let first = scan(&fx.root);
    let name = node_path(&first.snapshot, id_of(&first.snapshot, "a"))
        .unwrap()
        .into_os_string()
        .into_encoded_bytes();
    let reader = first.aggregator.reader();
    let a = id_of(&first.snapshot, "a");
    let epoch_before = reader.load().epoch();
    let (sent_tx, sent_rx) = mpsc::sync_channel(1);
    let (go_tx, go_rx) = mpsc::sync_channel(1);
    let scanner = Arc::new(Scripted {
        root_name: name,
        ending: Ending::Complete,
        sent: Some(sent_tx),
        go: std::sync::Mutex::new(Some(go_rx)),
    });
    let path = node_path(&first.snapshot, a).unwrap();
    let handle = start_rescan(
        first.aggregator,
        a,
        scanner,
        path,
        ScanOptions {
            publish_interval: Duration::from_millis(1),
            ..ScanOptions::default()
        },
    )
    .map_err(|r| r.error)
    .unwrap();
    sent_rx.recv_timeout(Duration::from_secs(30)).unwrap();
    // The reader published while the scan is part way.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mid = loop {
        let snap = reader.load();
        if snap.epoch() > epoch_before && snap.node(a).is_some_and(|n| n.is_in_progress()) {
            break snap;
        }
        assert!(Instant::now() < deadline, "no snapshot of the rescan");
        std::thread::sleep(Duration::from_millis(2));
    };
    assert!(!handle.is_done());
    mid.verify().unwrap();
    assert!(mid.root().unwrap().is_in_progress());
    assert!(!mid.scan_complete());
    assert!(mid.node(id_of(&mid, "b")).unwrap().is_complete());
    go_tx.send(()).unwrap();
    let done_by = Instant::now() + Duration::from_secs(20);
    while !handle.is_done() {
        assert!(Instant::now() < done_by);
        std::thread::sleep(Duration::from_millis(2));
    }
    let after = reader.load();
    after.verify().unwrap();
    assert!(after.node(a).unwrap().is_complete());
    assert!(after.scan_complete());
    let result = handle.wait();
    assert!(result.snapshot.epoch() >= after.epoch());
    assert_eq!(result.aggregator.rescan_report().unwrap().node, a);
}
