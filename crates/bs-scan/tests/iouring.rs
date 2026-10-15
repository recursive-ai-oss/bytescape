//! The io_uring variant of the walker (benchmark build only) must produce
//! exactly the walker's tree, and must fall back cleanly when the kernel
//! cannot do it.
#![cfg(all(target_os = "linux", feature = "iouring-bench"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use bs_scan::iouring_bench::{
    Config, IoUringWalker, probe, set_force_ring_failure, set_force_unavailable, take_stats,
};
use bs_scan::{ScanOptions, ScanResult, start};
use bs_tree::{NodeKind, NodeRef};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use support::{Fixture, assert_clean};

/// The statics the tests switch (forced failures, counters) are process-wide.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// `true` when the ring can be used here. Where it cannot, the tests that
/// need it say so and return, unless `BYTESCAPE_REQUIRE_IOURING` is set,
/// which makes that a failure.
fn ring_available(test: &str) -> bool {
    match probe() {
        Ok(()) => true,
        Err(e) => {
            assert!(
                std::env::var_os("BYTESCAPE_REQUIRE_IOURING").is_none(),
                "{test}: BYTESCAPE_REQUIRE_IOURING is set but io_uring statx is unavailable: {e:?}"
            );
            eprintln!("NOT RUN: {test}: io_uring statx unavailable here ({e:?})");
            false
        }
    }
}

type Row = (NodeKind, u64, u64, i64, u16, u8, bool, bool);

/// Every node by path, with everything the walker records for it.
fn flatten(result: &ScanResult) -> BTreeMap<Vec<u8>, Row> {
    let mut out = BTreeMap::new();
    let root = result.snapshot.root().unwrap();
    let mut stack: Vec<(NodeRef<'_>, Vec<u8>)> = vec![(root, Vec::new())];
    out.insert(Vec::new(), row(root));
    while let Some((node, path)) = stack.pop() {
        for child in node.children() {
            let mut p = path.clone();
            p.push(b'/');
            p.extend_from_slice(child.name());
            out.insert(p.clone(), row(child));
            stack.push((child, p));
        }
    }
    out
}

fn row(n: NodeRef<'_>) -> Row {
    (
        n.kind(),
        n.logical(),
        n.allocated(),
        n.mtime(),
        n.flags().bits(),
        n.category().0,
        n.is_unreadable(),
        n.hard_link_origin().is_some(),
    )
}

fn run(scanner: Arc<dyn bs_scan::Scanner>, root: &Path, threads: usize) -> ScanResult {
    let options = ScanOptions {
        threads: Some(threads),
        ..ScanOptions::default()
    };
    start(scanner, root.to_path_buf(), options).wait()
}

fn plain(root: &Path, threads: usize) -> ScanResult {
    run(Arc::new(bs_scan::Walker), root, threads)
}

fn sh(script: &str) {
    let status = Command::new("bash").args(["-ec", script]).status().unwrap();
    assert!(status.success(), "{script}");
}

/// A tree with the kinds of entry the walker handles, one directory big
/// enough for several listing buffers and several batches.
fn fixture(name: &str, cross_dir_links: bool) -> Fixture {
    let fx = Fixture::new(name);
    let r = fx.root.display();
    sh(&format!(
        r#"cd '{r}'
mkdir -p a/b/c empty-dir big sparse-dir
seq 1 7000 | sed 's|^|big/file-|' | xargs touch
for i in $(seq 1 91 7000); do head -c $((i % 3000)) /dev/zero > big/file-$i; done
head -c 5000 /dev/urandom > a/five-k
head -c 300000 /dev/urandom > a/b/three-hundred-k
truncate -s 50M sparse-dir/sparse
dd if=/dev/urandom of=sparse-dir/sparse bs=4096 count=1 seek=100 conv=notrunc status=none
ln a/five-k a/five-k-link
ln -s a/five-k link-to-file
ln -s a link-to-dir
ln -s /nonexistent dangling
mkfifo fifo
printf z > "$(printf 'odd-\377-name')"
printf z > "$(printf 'new\nline')"
printf z > "$(printf 'n%.0s' $(seq 1 250))"
mkdir -p "$(printf 'd%.0s' $(seq 1 200))"
printf q > a/b/c/leaf
"#
    ));
    if cross_dir_links {
        sh(&format!(
            "cd '{}'; ln a/b/three-hundred-k big/linked-elsewhere",
            fx.root.display()
        ));
    }
    // A directory nobody can read (the check is meaningless as root).
    sh(&format!(
        "cd '{r}'; mkdir locked; printf s > locked/inside; chmod 000 locked"
    ));
    fx.settle();
    fx
}

fn is_root() -> bool {
    String::from_utf8_lossy(&Command::new("id").arg("-u").output().unwrap().stdout).trim() == "0"
}

fn configs() -> Vec<Config> {
    let mut v = Vec::new();
    for depth in [1u32, 7, 64, 512] {
        for force_async in [false, true] {
            v.push(Config { depth, force_async });
        }
    }
    v
}

#[test]
fn the_ring_variant_gives_the_walkers_tree_on_one_thread() {
    let _g = lock();
    if !ring_available("the_ring_variant_gives_the_walkers_tree_on_one_thread") {
        return;
    }
    let fx = fixture("uring-equal-1", true);
    let want = plain(&fx.root, 1);
    assert_clean_but_locked(&want);
    let want_rows = flatten(&want);
    assert!(want_rows.len() > 7000);
    for config in configs() {
        let walker = Arc::new(IoUringWalker::new(config));
        let _ = take_stats();
        let got = run(walker.clone(), &fx.root, 1);
        assert!(!walker.fell_back(), "{config:?}");
        assert_clean_but_locked(&got);
        let stats = take_stats();
        assert!(stats.requests > 7000, "{config:?}: {stats:?}");
        assert_eq!(stats.thread_fallbacks, 0);
        assert_eq!(flatten(&got), want_rows, "{config:?}");
        // How many batches the records went out in depends on timing.
        let (mut got_p, mut want_p) = (got.stats.progress, want.stats.progress);
        got_p.batches = 0;
        want_p.batches = 0;
        assert_eq!(got_p, want_p, "{config:?}");
    }
}

#[test]
fn the_ring_variant_gives_the_walkers_tree_on_many_threads() {
    let _g = lock();
    if !ring_available("the_ring_variant_gives_the_walkers_tree_on_many_threads") {
        return;
    }
    // Links between directories are counted at whichever name a thread meets
    // first, so this tree has none.
    let fx = fixture("uring-equal-n", false);
    let want_rows = flatten(&plain(&fx.root, 8));
    for config in [
        Config {
            depth: 64,
            force_async: false,
        },
        Config {
            depth: 256,
            force_async: true,
        },
    ] {
        let walker = Arc::new(IoUringWalker::new(config));
        let got = run(walker.clone(), &fx.root, 8);
        assert!(!walker.fell_back());
        assert_eq!(flatten(&got), want_rows, "{config:?}");
    }
}

#[test]
fn batching_makes_fewer_system_calls_than_requests() {
    let _g = lock();
    if !ring_available("batching_makes_fewer_system_calls_than_requests") {
        return;
    }
    let fx = fixture("uring-batch", false);
    let walker = Arc::new(IoUringWalker::new(Config {
        depth: 256,
        force_async: false,
    }));
    let _ = take_stats();
    let _ = run(walker, &fx.root, 1);
    let s = take_stats();
    assert!(s.requests > 7000);
    assert!(
        s.enters * 20 < s.requests,
        "one call per batch, not per name: {s:?}"
    );
}

#[test]
fn when_the_kernel_cannot_do_it_the_plain_walker_runs_and_says_so() {
    let _g = lock();
    let fx = fixture("uring-fallback", false);
    let want_rows = flatten(&plain(&fx.root, 4));

    // The probe fails: no ring is ever made, and the answer is the walker's.
    set_force_unavailable(true);
    let _ = take_stats();
    let walker = Arc::new(IoUringWalker::new(Config::default()));
    let got = run(walker.clone(), &fx.root, 4);
    set_force_unavailable(false);
    assert!(walker.fell_back());
    assert_eq!(take_stats().requests, 0);
    assert_eq!(flatten(&got), want_rows);
    assert!(probe().is_ok() || std::env::var_os("BYTESCAPE_REQUIRE_IOURING").is_none());

    // The probe passes but a worker thread cannot get a ring: that thread
    // examines its entries one by one, and the tree is still the walker's.
    if !ring_available("when_the_kernel_cannot_do_it (per-thread leg)") {
        return;
    }
    set_force_ring_failure(true);
    let _ = take_stats();
    let walker = Arc::new(IoUringWalker::new(Config::default()));
    let got = run(walker.clone(), &fx.root, 4);
    set_force_ring_failure(false);
    let s = take_stats();
    assert!(!walker.fell_back(), "the probe itself passed");
    assert_eq!(s.requests, 0);
    assert!(s.thread_fallbacks >= 1, "{s:?}");
    assert_eq!(flatten(&got), want_rows);
}

#[test]
fn an_unreadable_directory_is_recorded_the_same_way() {
    let _g = lock();
    if is_root() || !ring_available("an_unreadable_directory_is_recorded_the_same_way") {
        return;
    }
    let fx = fixture("uring-locked", false);
    let got = run(Arc::new(IoUringWalker::new(Config::default())), &fx.root, 2);
    let rows = flatten(&got);
    let locked = rows.get(b"/locked".as_slice()).expect("locked is listed");
    assert!(locked.6, "the directory nobody can read is marked");
    assert!(!rows.contains_key(b"/locked/inside".as_slice()));
}

#[test]
fn names_are_kept_byte_for_byte() {
    let _g = lock();
    if !ring_available("names_are_kept_byte_for_byte") {
        return;
    }
    let fx = fixture("uring-names", false);
    let got = run(Arc::new(IoUringWalker::new(Config::default())), &fx.root, 2);
    let rows = flatten(&got);
    assert!(rows.contains_key(b"/odd-\xff-name".as_slice()));
    assert!(rows.contains_key(b"/new\nline".as_slice()));
}

/// The fixture has a directory nobody can read, so a finished scan of it is
/// complete but marked; everything else `assert_clean` checks still holds.
fn assert_clean_but_locked(result: &ScanResult) {
    if is_root() {
        assert_clean(result);
    } else {
        assert!(result.stats.error.is_none());
        assert_eq!(result.stats.records_parked, 0);
        assert_eq!(result.stats.records_rejected, 0);
        result.snapshot.verify().unwrap();
    }
}
