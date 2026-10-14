//! The Linux walker against fixture trees, each checked against GNU `du` where
//! the tree is quiet enough for that to mean something.
#![cfg(target_os = "linux")]

mod support;

use bs_scan::{Boundary, ScanContext, ScanEnd, ScanOptions, Scanner, Walker};
use bs_tree::{Aggregator, Batch, DirKey, NodeFlags, NodeKind, ReadErrorKind, Record};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::process::Command;
use std::sync::Arc;
use support::*;

#[test]
fn an_ordinary_tree_matches_du() {
    let fx = Fixture::new("ordinary");
    fx.file("a.txt", 10);
    fx.file("b/c.bin", 5000);
    fx.file("b/d/e.bin", 123_456);
    fx.dir("b/empty");
    fx.file("z/y/x/w.txt", 1);
    fx.file("big.dat", 3_000_000);
    fx.settle();
    let result = scan(&fx.root);
    assert_clean(&result);
    assert_matches_du(&fx.root, &result);

    let snap = &result.snapshot;
    assert_eq!(snap.root().unwrap().name(), fx.root.as_os_str().as_bytes());
    let e = node_at(snap, "b/d/e.bin");
    assert_eq!(e.kind(), NodeKind::File);
    assert_eq!(e.logical(), 123_456);
    assert_eq!(e.allocated() % 4096, 0);
    assert!(e.mtime() > 1_600_000_000);
    assert_eq!(node_at(snap, "b/empty").kind(), NodeKind::Dir);
    assert!(node_at(snap, "b/empty").is_complete());
    assert_eq!(snap.root().unwrap().file_count(), 5);
    // The counters agree with the tree for a tree without hard links.
    let p = result.stats.progress;
    assert_eq!(p.files, 5);
    assert_eq!(p.dirs, 7);
    assert_eq!(p.allocated_bytes, snap.root().unwrap().allocated());
    assert_eq!(p.logical_bytes, snap.root().unwrap().logical());
    assert_eq!(p.unreadable, 0);
}

#[test]
fn hard_links_are_counted_once_and_flagged() {
    let fx = Fixture::new("hardlinks");
    fx.file("d1/original", 20_000);
    fx.dir("d2");
    fx.dir("d3/deeper");
    std::fs::hard_link(fx.path("d1/original"), fx.path("d2/second")).unwrap();
    std::fs::hard_link(fx.path("d1/original"), fx.path("d3/deeper/third")).unwrap();
    // A second file with two names, one of them outside the scanned root.
    fx.file("d1/solo", 9_000);
    let outside = Fixture::new("hardlinks-outside");
    std::fs::hard_link(fx.path("d1/solo"), outside.path("elsewhere")).unwrap();
    fx.file("plain", 4_000);
    fx.settle();

    let result = scan(&fx.root);
    assert_clean(&result);
    assert_matches_du(&fx.root, &result);

    // The live progress counters add every name, as documented on
    // `ScanProgress`; only the snapshot holds the de-duplicated totals.
    let progress = result.stats.progress;
    let root = result.snapshot.root().unwrap();
    assert_eq!(progress.logical_bytes, 3 * 20_000 + 9_000 + 4_000);
    assert_eq!(root.logical(), 20_000 + 9_000 + 4_000);
    assert!(
        progress.allocated_bytes > root.allocated(),
        "progress counted the two extra names of the linked file"
    );

    let snap = &result.snapshot;
    let names = ["d1/original", "d2/second", "d3/deeper/third"];
    let nodes: Vec<_> = names.iter().map(|n| node_at(snap, n)).collect();
    assert!(nodes.iter().all(|n| n.is_hard_link()));
    let counted: Vec<_> = nodes.iter().filter(|n| n.allocated() > 0).collect();
    assert_eq!(counted.len(), 1, "bytes are counted at exactly one name");
    assert_eq!(nodes.iter().map(|n| n.logical()).sum::<u64>(), 20_000);
    let dups = nodes
        .iter()
        .filter(|n| n.flags().contains(NodeFlags::HARD_LINK_DUPLICATE))
        .count();
    assert_eq!(dups, 2);
    // Every name counts as an entry.
    assert_eq!(snap.root().unwrap().file_count(), 5);

    // Only one of its names is inside the scan, and it is still a hard link.
    let solo = node_at(snap, "d1/solo");
    assert!(solo.is_hard_link());
    assert_eq!(solo.logical(), 9_000);
    assert!(!solo.flags().contains(NodeFlags::HARD_LINK_DUPLICATE));
    assert!(!node_at(snap, "plain").is_hard_link());
}

#[test]
fn sparse_files_report_allocated_below_logical() {
    let fx = Fixture::new("sparse");
    let p = fx.path("sparse.img");
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::File::create(&p).unwrap();
        f.set_len(256 << 20).unwrap();
        f.seek(SeekFrom::Start(100 << 20)).unwrap();
        f.write_all(&[7u8; 8192]).unwrap();
        f.sync_all().unwrap();
    }
    fx.file("dense.bin", 70_000);
    fx.settle();
    let result = scan(&fx.root);
    assert_clean(&result);
    assert_matches_du(&fx.root, &result);
    let sparse = node_at(&result.snapshot, "sparse.img");
    assert_eq!(sparse.logical(), 256 << 20);
    assert!(sparse.allocated() < (1 << 20), "{}", sparse.allocated());
    assert!(sparse.allocated() >= 8192);
    assert!(sparse.flags().contains(NodeFlags::SPARSE_OR_COMPRESSED));
    assert!(
        !node_at(&result.snapshot, "dense.bin")
            .flags()
            .contains(NodeFlags::SPARSE_OR_COMPRESSED)
    );
}

#[test]
fn links_are_recorded_as_links_and_never_followed() {
    let fx = Fixture::new("symlinks");
    let outside = Fixture::new("symlinks-outside");
    // A big tree and a big file the links point at.
    outside.file("huge/inner.bin", 8_000_000);
    outside.file("big.bin", 9_000_000);
    fx.file("real/file.txt", 100);
    symlink(&outside.root, fx.path("to-dir")).unwrap();
    symlink(outside.path("big.bin"), fx.path("to-file")).unwrap();
    symlink("does-not-exist", fx.path("dangling")).unwrap();
    symlink("loop-b", fx.path("loop-a")).unwrap();
    symlink("loop-a", fx.path("loop-b")).unwrap();
    symlink(".", fx.path("real/self")).unwrap();
    symlink("..", fx.path("real/up")).unwrap();
    fx.settle();

    let result = scan(&fx.root);
    assert_clean(&result);
    assert_matches_du(&fx.root, &result);

    let snap = &result.snapshot;
    for name in [
        "to-dir",
        "to-file",
        "dangling",
        "loop-a",
        "loop-b",
        "real/self",
        "real/up",
    ] {
        let n = node_at(snap, name);
        assert_eq!(n.kind(), NodeKind::Symlink, "{name}");
        assert!(n.flags().contains(NodeFlags::NOT_FOLLOWED), "{name}");
        assert_eq!(n.children().count(), 0, "{name}");
    }
    assert!(
        snap.root().unwrap().logical() < 100_000,
        "nothing was followed"
    );
    // A link counts as the link itself: its size is the length of its target.
    assert_eq!(
        node_at(snap, "dangling").logical(),
        "does-not-exist".len() as u64
    );
}

#[test]
fn an_unreadable_directory_is_recorded_and_the_scan_completes() {
    if is_root() {
        eprintln!("skipped: running as root, where chmod 000 does not deny access");
        return;
    }
    let fx = Fixture::new("unreadable");
    fx.file("open/a.txt", 1000);
    fx.file("locked/secret.txt", 50_000);
    fx.file("locked/inner/more.txt", 50_000);
    fx.file("other/b.txt", 2000);
    // No list/search permission: it can be examined from outside but not opened.
    std::fs::set_permissions(fx.path("locked"), std::fs::Permissions::from_mode(0o000)).unwrap();
    fx.settle();

    let result = scan(&fx.root);
    assert_eq!(result.stats.error.as_ref().map(|e| e.to_string()), None);
    assert_eq!(result.stats.end, Some(ScanEnd::Completed));
    assert_eq!(result.stats.records_parked, 0);
    let snap = &result.snapshot;
    snap.verify().unwrap();
    assert!(
        snap.scan_complete(),
        "unreadable directories do not stop completion"
    );

    let locked = node_at(snap, "locked");
    assert!(locked.is_unreadable());
    assert!(locked.is_complete());
    assert_eq!(
        locked.read_failure().unwrap().kind,
        ReadErrorKind::PermissionDenied
    );
    assert_eq!(locked.children().count(), 0);
    let root = snap.root().unwrap();
    assert!(root.has_unreadable_below());
    assert!(root.bytes_are_lower_bound(), "totals are a lower bound");
    assert!(!node_at(snap, "open").bytes_are_lower_bound());
    assert_eq!(result.stats.progress.unreadable, 1);
    // What could be read is counted.
    assert_eq!(node_at(snap, "open/a.txt").logical(), 1000);
    assert_eq!(node_at(snap, "other/b.txt").logical(), 2000);
}

/// A chain of nested directories as deep as asked, built through
/// `/proc/self/fd` so that no path is longer than the system allows.
fn build_chain(base: &std::path::Path, depth: usize) -> std::fs::File {
    let mut held = std::fs::File::open(base).unwrap();
    for level in 0..depth {
        use std::os::fd::AsRawFd;
        let at = format!("/proc/self/fd/{}", held.as_raw_fd());
        std::fs::create_dir(format!("{at}/d")).unwrap();
        if level % 500 == 0 {
            std::fs::write(format!("{at}/d/file{level}"), vec![b'f'; 1000]).unwrap();
        }
        held = std::fs::File::open(format!("{at}/d")).unwrap();
    }
    held
}

#[test]
fn a_tree_deeper_than_a_path_can_name_is_walked_without_recursion() {
    const DEPTH: usize = 6000;
    let fx = Fixture::new("deep");
    let bottom = build_chain(&fx.root, DEPTH);
    drop(bottom);
    fx.settle();

    let result = scan(&fx.root);
    assert_clean(&result);
    let snap = &result.snapshot;
    // The root, DEPTH directories, and a file every 500 levels.
    let files = DEPTH.div_ceil(500);
    assert_eq!(snap.len() as usize, 1 + DEPTH + files);
    assert_eq!(snap.root().unwrap().file_count() as usize, files);
    const { assert!(DEPTH * 2 > 4096, "the path to the bottom exceeds PATH_MAX") };
    // A deep chain goes out in a few large batches, not one per directory.
    assert!(
        result.stats.progress.batches < 100,
        "{} batches for {} directories",
        result.stats.progress.batches,
        DEPTH
    );
    assert_matches_du(&fx.root, &result);
}

#[test]
fn names_that_are_not_text_survive_exactly() {
    let fx = Fixture::new("names");
    let odd: Vec<&[u8]> = vec![
        b"plain",
        b"with space",
        b"new\nline",
        b"tab\there",
        b"bad\xff\xfeutf8",
        b"\xc0\xaf",
        b"-leading-dash",
        b"back\\slash",
        b"quote\"s",
        b"\xe2\x80\xae-rtl",
        b".hidden",
        &[b'l'; 255],
    ];
    for (i, name) in odd.iter().enumerate() {
        let p = fx.root.join(OsStr::from_bytes(name));
        std::fs::write(&p, vec![b'x'; 100 + i]).unwrap();
    }
    // Also a directory with an odd name.
    std::fs::create_dir(fx.root.join(OsStr::from_bytes(b"dir\xffname"))).unwrap();
    std::fs::write(
        fx.root
            .join(OsStr::from_bytes(b"dir\xffname"))
            .join("inner"),
        b"hi",
    )
    .unwrap();
    fx.settle();

    let result = scan(&fx.root);
    assert_clean(&result);
    assert_matches_du(&fx.root, &result);
    let root = result.snapshot.root().unwrap();
    let found: HashSet<Vec<u8>> = root.children().map(|c| c.name().to_vec()).collect();
    for name in &odd {
        assert!(found.contains(*name), "{:?}", String::from_utf8_lossy(name));
    }
    assert!(found.contains(&b"dir\xffname"[..]));
    let dir = root
        .children()
        .find(|c| c.name() == b"dir\xffname")
        .unwrap();
    assert_eq!(dir.children().next().unwrap().name(), b"inner");
    // The path round-trips as raw bytes.
    let mut want = fx.root.as_os_str().as_bytes().to_vec();
    want.extend_from_slice(b"/dir\xffname/inner");
    let inner = dir.children().next().unwrap().id();
    assert_eq!(result.snapshot.path(inner, b'/').unwrap(), want);
}

#[test]
fn special_files_are_other_and_empty_roots_complete() {
    let fx = Fixture::new("special");
    assert!(
        Command::new("mkfifo")
            .arg(fx.path("pipe"))
            .status()
            .unwrap()
            .success()
    );
    fx.file("file", 10);
    fx.settle();
    let result = scan(&fx.root);
    assert_clean(&result);
    assert_matches_du(&fx.root, &result);
    assert_eq!(node_at(&result.snapshot, "pipe").kind(), NodeKind::Other);

    let empty = Fixture::new("empty");
    let result = scan(&empty.root);
    assert_clean(&result);
    assert_eq!(result.snapshot.len(), 1);
    assert!(result.snapshot.root().unwrap().is_complete());
}

#[test]
fn the_root_may_be_a_link_but_not_a_file() {
    let fx = Fixture::new("root-kinds");
    fx.file("real/f", 100);
    symlink(fx.path("real"), fx.path("alias")).unwrap();
    let result = scan(&fx.path("alias"));
    assert_clean(&result);
    // The root is resolved once; the record carries the real path.
    assert_eq!(
        result.snapshot.root().unwrap().name(),
        fx.path("real")
            .canonicalize()
            .unwrap()
            .as_os_str()
            .as_bytes()
    );
    assert_eq!(node_at(&result.snapshot, "f").logical(), 100);

    let result = scan(&fx.path("real/f"));
    assert!(matches!(
        result.stats.error,
        Some(bs_scan::ScanError::NotADirectory(_))
    ));
    assert!(result.snapshot.is_empty());
    let result = scan(&fx.path("missing"));
    assert!(matches!(
        result.stats.error,
        Some(bs_scan::ScanError::Root(_))
    ));
}

#[test]
fn every_thread_count_gives_the_same_answer() {
    let fx = Fixture::new("threads");
    for d in 0..40 {
        for f in 0..25 {
            fx.file(&format!("d{d}/sub{}/f{f}", f % 4), 100 * (f + 1));
        }
    }
    fx.settle();
    let want_alloc = du(&fx.root, false).expect("GNU du");
    for threads in [1, 2, 3, 8, 32] {
        let result = scan_with(
            &fx.root,
            ScanOptions {
                threads: Some(threads),
                ..ScanOptions::default()
            },
        );
        assert_clean(&result);
        assert_eq!(result.stats.threads, threads);
        assert_eq!(
            result.snapshot.root().unwrap().allocated(),
            want_alloc,
            "{threads} threads"
        );
    }
}

// ---- the producer contract with the tree -----------------------------------

/// Runs the walker into a plain channel, so the batches themselves can be
/// examined.
fn collect_batches(root: &std::path::Path, options: ScanOptions) -> (Vec<Batch>, ScanEnd) {
    let (ctx, rx) = ScanContext::channel(options, 8);
    let root = root.to_path_buf();
    // The context moves into the thread; when the walk ends it is dropped,
    // which closes the channel.
    let walker = std::thread::spawn(move || Walker.scan(&root, &ctx).unwrap());
    let batches: Vec<Batch> = rx.iter().collect();
    (batches, walker.join().unwrap())
}

#[test]
fn directory_keys_are_unique_and_parents_are_always_sent_first() {
    let fx = Fixture::new("keys");
    for a in 0..60 {
        for b in 0..40 {
            fx.file(&format!("a{a}/b{b}/leaf"), 10);
        }
    }
    fx.settle();
    let (batches, end) = collect_batches(&fx.root, ScanOptions::default());
    assert_eq!(end, ScanEnd::Completed);

    let mut seen_keys: HashSet<u32> = HashSet::new();
    let mut dirs = 0usize;
    let mut listing_done: HashSet<u32> = HashSet::new();
    for batch in &batches {
        for record in batch.records() {
            match record {
                Record::Entry(e) => {
                    if e.parent != DirKey::NONE {
                        assert!(
                            seen_keys.contains(&e.parent.0),
                            "an entry arrived before its parent directory"
                        );
                        assert!(
                            !listing_done.contains(&e.parent.0),
                            "entry after listing done"
                        );
                    }
                    if e.kind == NodeKind::Dir {
                        assert!(seen_keys.insert(e.key.0), "key {} reused", e.key.0);
                        assert_ne!(e.key, DirKey::NONE);
                        dirs += 1;
                    }
                }
                Record::ListingDone { dir } => {
                    assert!(listing_done.insert(dir.0), "listing finished twice");
                }
                Record::Unreadable { .. } => {}
            }
        }
    }
    assert_eq!(dirs, 1 + 60 + 60 * 40);
    assert_eq!(listing_done.len(), dirs, "every directory was finished");

    // Applied in the order they were sent, the tree needs to hold nothing back.
    let mut agg = Aggregator::new();
    for batch in &batches {
        let report = agg.apply(batch);
        assert_eq!((report.parked, report.rejected), (0, 0));
    }
    agg.finish();
    assert!(agg.publish().scan_complete());
}

#[test]
fn exhausted_directory_keys_are_recorded_not_dropped() {
    let fx = Fixture::new("keys-exhausted");
    for i in 0..8 {
        fx.file(&format!("d{i}/f"), 100);
    }
    fx.settle();
    let (ctx, rx) = ScanContext::channel(ScanOptions::default(), 64);
    ctx.set_first_dir_key(u32::MAX - 5);
    let end = Walker.scan(&fx.root, &ctx).unwrap();
    drop(ctx);
    assert_eq!(end, ScanEnd::Completed);
    let mut agg = Aggregator::new();
    for batch in rx.iter() {
        assert_eq!(agg.apply(&batch).rejected, 0);
    }
    agg.finish();
    let snap = agg.publish();
    snap.verify().unwrap();
    let root = snap.root().unwrap();
    // Four keys were left after the root's; the other four directories could
    // not be given one and are recorded as unread entries.
    let unread: Vec<_> = root.children().filter(|c| c.is_unreadable()).collect();
    assert_eq!(unread.len(), 4);
    assert!(unread.iter().all(|c| c.kind() == NodeKind::Other));
    assert!(root.bytes_are_lower_bound());
    assert_eq!(root.children().count(), 8);
}

// ---- cancel and live snapshots ----------------------------------------------

#[test]
fn a_scan_cancelled_before_it_starts_keeps_the_root_and_is_incomplete() {
    let fx = Fixture::new("cancel-early");
    for d in 0..20 {
        fx.file(&format!("d{d}/f"), 10);
    }
    let (ctx, rx) = ScanContext::channel(ScanOptions::default(), 64);
    ctx.cancel();
    let end = Walker.scan(&fx.root, &ctx).unwrap();
    drop(ctx);
    assert_eq!(end, ScanEnd::Cancelled);
    let mut agg = Aggregator::new();
    for batch in rx.iter() {
        assert_eq!(agg.apply(&batch).rejected, 0);
    }
    agg.mark_stopped();
    agg.finish();
    let snap = agg.publish();
    snap.verify().unwrap();
    assert!(snap.scan_stopped());
    assert!(!snap.scan_complete());
    assert!(snap.root().unwrap().is_in_progress());
    assert!(snap.root().unwrap().bytes_are_lower_bound());
}

#[test]
fn cancelling_a_running_scan_keeps_valid_partial_results() {
    let fx = Fixture::new("cancel-running");
    for a in 0..80 {
        for b in 0..60 {
            fx.file(&format!("a{a}/b{b}/f"), 1);
        }
    }
    fx.settle();
    let handle = bs_scan::start(
        Arc::new(Walker),
        fx.root.clone(),
        ScanOptions {
            threads: Some(2),
            ..ScanOptions::default()
        },
    );
    while handle.progress().dirs < 50 && !handle.is_done() {
        std::thread::yield_now();
    }
    handle.cancel();
    let result = handle.wait();
    result
        .snapshot
        .verify()
        .expect("partial tree is consistent");
    assert_eq!(result.stats.records_parked, 0);
    match result.stats.end {
        Some(ScanEnd::Cancelled) => {
            assert!(result.snapshot.scan_stopped());
            assert!(!result.snapshot.scan_complete());
            assert!(result.snapshot.root().unwrap().is_in_progress());
            // What was counted before the stop is in the tree.
            assert!(result.snapshot.len() > 1);
        }
        // The scan finished before the cancel landed.
        Some(ScanEnd::Completed) => assert!(result.snapshot.scan_complete()),
        None => panic!("scan failed: {:?}", result.stats.error),
    }
}

#[test]
fn readers_see_consistent_snapshots_while_the_scan_runs() {
    let fx = Fixture::new("live");
    for a in 0..100 {
        for b in 0..50 {
            fx.file(&format!("a{a}/b{b}/f"), 100);
        }
    }
    fx.settle();
    let handle = bs_scan::start(
        Arc::new(Walker),
        fx.root.clone(),
        ScanOptions {
            publish_interval: std::time::Duration::from_millis(5),
            os_used_bytes: Some(1 << 40),
            ..ScanOptions::default()
        },
    );
    let reader = handle.reader();
    let mut epochs = HashSet::new();
    let mut last_alloc = 0;
    while !handle.is_done() {
        let snap = reader.load();
        epochs.insert(snap.epoch());
        snap.verify()
            .expect("every published snapshot is consistent");
        if let Some(root) = snap.root() {
            assert!(root.allocated() >= last_alloc, "totals only grow");
            last_alloc = root.allocated();
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    let result = handle.wait();
    assert_clean(&result);
    assert!(
        epochs.len() > 1,
        "the map filled in while the scan ran: {epochs:?}"
    );
    assert_eq!(result.snapshot.os_used_bytes(), Some(1 << 40));
    assert!(matches!(result.snapshot.progress(), bs_tree::Progress::Ratio(r) if r == 1.0));
}

#[test]
fn a_directory_swapped_for_a_link_mid_scan_is_never_followed() {
    let fx = Fixture::new("swap");
    let outside = Fixture::new("swap-outside");
    // Following the link would add a terabyte (logical).
    let big = std::fs::File::create(outside.path("trap.bin")).unwrap();
    big.set_len(1 << 40).unwrap();
    for d in 0..300 {
        fx.file(&format!("d{d}/f"), 100);
        fx.dir(&format!("d{d}/victim"));
    }
    fx.settle();

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let swapper = {
        let stop = Arc::clone(&stop);
        let root = fx.root.clone();
        let target = outside.root.clone();
        std::thread::spawn(move || {
            let mut i = 0u32;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let d = root.join(format!("d{}", i % 300));
                let victim = d.join("victim");
                let moved = d.join("victim.old");
                if std::fs::rename(&victim, &moved).is_ok() {
                    let _ = symlink(&target, &victim);
                    let _ = std::fs::remove_file(&victim);
                    let _ = std::fs::rename(&moved, &victim);
                }
                i += 1;
            }
        })
    };
    for _ in 0..20 {
        let result = scan(&fx.root);
        assert!(result.stats.error.is_none());
        result.snapshot.verify().unwrap();
        assert!(
            result.snapshot.root().unwrap().logical() < (1 << 30),
            "a swapped-in link was followed: {} logical bytes",
            result.snapshot.root().unwrap().logical()
        );
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    swapper.join().unwrap();
}

// ---- the mount boundary -------------------------------------------------------

/// `(mount id, mount point)` of every mount, from the kernel's table.
fn mounts() -> Vec<(u64, String)> {
    let text = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    text.lines()
        .filter_map(|l| {
            let mut f = l.split(' ');
            let id = f.next()?.parse().ok()?;
            let point = f.nth(3)?.replace("\\040", " ");
            Some((id, point))
        })
        .collect()
}

#[test]
fn another_mount_is_not_entered_unless_the_boundary_allows_it() {
    // /dev holds mounts of other filesystems (devpts, tmpfs, mqueue, ...)
    // directly below it. Reading is enough; nothing is mounted by the test.
    let below: Vec<(u64, String)> = mounts()
        .into_iter()
        .filter(|(_, p)| p.starts_with("/dev/") && p.matches('/').count() == 2)
        .collect();
    let Some((mount_id, point)) = below.first().cloned() else {
        eprintln!("skipped: no mount directly below /dev on this machine");
        return;
    };
    let name = point.trim_start_matches("/dev/").to_string();

    // The default boundary is the root's own mount: the child mount is a
    // closed, empty, flagged directory.
    let result = scan(std::path::Path::new("/dev"));
    result.snapshot.verify().unwrap();
    let child = node_at(&result.snapshot, &name);
    assert_eq!(child.kind(), NodeKind::Dir);
    assert!(child.flags().contains(NodeFlags::NOT_FOLLOWED), "{name}");
    assert!(child.is_complete());
    assert_eq!(child.children().count(), 0);
    assert_eq!(
        (child.allocated(), child.logical()),
        (0, 0),
        "nothing of another drive is counted"
    );

    // A boundary that lists the mount enters it. (The root's own mount is
    // always inside.)
    let entered = scan_with(
        std::path::Path::new("/dev"),
        ScanOptions {
            boundary: Boundary::with_mount_ids([mount_id]),
            ..ScanOptions::default()
        },
    );
    entered.snapshot.verify().unwrap();
    let child = node_at(&entered.snapshot, &name);
    assert!(!child.flags().contains(NodeFlags::NOT_FOLLOWED), "{name}");
    assert!(child.is_complete());
    // Other mounts below /dev stay closed.
    for (_, other) in below.iter().filter(|(id, _)| *id != mount_id) {
        let n = node_at(&entered.snapshot, other.trim_start_matches("/dev/"));
        assert!(n.flags().contains(NodeFlags::NOT_FOLLOWED), "{other}");
    }
}

#[test]
fn the_shared_memory_mount_below_dev_is_a_different_filesystem_and_stays_closed() {
    let shm = std::path::Path::new("/dev/shm");
    if !shm.is_dir() {
        eprintln!("skipped: no /dev/shm");
        return;
    }
    let result = scan(std::path::Path::new("/dev"));
    let node = try_node_at(&result.snapshot, "shm");
    if let Some(n) = node {
        assert!(n.flags().contains(NodeFlags::NOT_FOLLOWED));
        assert_eq!(n.children().count(), 0);
    }
    let meta_dev = std::fs::metadata("/dev").unwrap().dev();
    let meta_shm = std::fs::metadata(shm).unwrap().dev();
    assert_ne!(meta_dev, meta_shm, "/dev/shm is its own filesystem");
}

#[test]
fn progress_against_the_systems_used_bytes() {
    let fx = Fixture::new("progress");
    fx.file("a", 100_000);
    fx.settle();
    // The real used-bytes figure of the volume is far above this tree.
    let used = bs_scan::VolumeInfo::used_bytes(&bs_scan::StatvfsVolume, &fx.root).unwrap();
    let result = scan_with(
        &fx.root,
        ScanOptions {
            os_used_bytes: Some(used),
            ..ScanOptions::default()
        },
    );
    assert_clean(&result);
    assert!(used > result.snapshot.root().unwrap().allocated());
    assert!(!result.snapshot.counted_exceeds_used());
    // A figure below what was counted means a ratio would mislead.
    let result = scan_with(
        &fx.root,
        ScanOptions {
            os_used_bytes: Some(1),
            ..ScanOptions::default()
        },
    );
    assert!(result.snapshot.counted_exceeds_used());
}
