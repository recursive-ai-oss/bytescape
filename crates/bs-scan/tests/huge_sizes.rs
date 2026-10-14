//! Sizes come from the disk, and a filesystem can report enormous ones: tmpfs,
//! XFS and btrfs allow sparse files of 2^63 - 1 bytes, so a few of them
//! overflow a 64-bit sum. The walker must count them as saturated, not wrap
//! and not panic.
//!
//! Making such files needs a filesystem that allows them. This host has no
//! such filesystem outside a private mount namespace, so the test runs its
//! second half under `unshare --user --map-root-user --mount` with a tmpfs of
//! its own. Where the namespace cannot be made the test says so and does not
//! pass for it: it prints `NOT RUN` and returns.
#![cfg(target_os = "linux")]

mod support;

use std::process::Command;
use support::*;

const HUGE: u64 = i64::MAX as u64;

/// Runs inside the namespace, where `BS_HUGE_DIR` is a fresh tmpfs. Outside
/// it (a plain `cargo test`) it has nothing to do.
#[test]
fn inside_the_namespace_huge_sparse_files_saturate() {
    let Some(dir) = std::env::var_os("BS_HUGE_DIR") else {
        return;
    };
    let base = std::path::Path::new(&dir).join("tree");
    std::fs::create_dir_all(base.join("a")).unwrap();
    for name in ["one", "two", "three"] {
        std::fs::File::create(base.join("a").join(name))
            .unwrap()
            .set_len(HUGE)
            .unwrap();
    }
    for threads in [1, 4] {
        let result = scan_with(
            &base,
            bs_scan::ScanOptions {
                threads: Some(threads),
                ..bs_scan::ScanOptions::default()
            },
        );
        assert!(
            result.stats.error.is_none(),
            "{threads} threads: {:?}",
            result.stats.error
        );
        assert_eq!(result.stats.end, Some(bs_scan::ScanEnd::Completed));
        result.snapshot.verify().unwrap();
        // Three names of 2^63 - 1 bytes are more than a u64 holds. The live
        // counter saturates; wrapping would leave it at 2^63 - 3.
        let progress = result.stats.progress;
        assert_eq!(progress.logical_bytes, u64::MAX, "{threads} threads");
        assert_eq!(progress.files, 3);
        // The tree's sum is not allowed to wrap either.
        let logical = result.snapshot.root().unwrap().logical();
        assert!(logical >= HUGE, "{threads} threads: tree logical {logical}");
    }
}

#[test]
fn huge_sparse_files_saturate_counters_in_a_private_tmpfs() {
    if std::env::var_os("BS_HUGE_DIR").is_some() {
        // This is the inner run; the other test does the work.
        return;
    }
    let probe = Command::new("unshare")
        .args(["--user", "--map-root-user", "--mount", "true"])
        .output();
    if !matches!(&probe, Ok(o) if o.status.success()) {
        eprintln!("NOT RUN: cannot create a user and mount namespace here");
        return;
    }
    let fx = Fixture::new("huge");
    let mnt = fx.dir("mnt");
    let exe = std::env::current_exe().unwrap();
    let script = format!(
        "mount -t tmpfs -o size=16m none '{}' && exec '{}' \
         inside_the_namespace_huge_sparse_files_saturate --exact --test-threads=1 --nocapture",
        mnt.display(),
        exe.display()
    );
    let out = Command::new("unshare")
        .args(["--user", "--map-root-user", "--mount", "sh", "-c", &script])
        .env("BS_HUGE_DIR", &mnt)
        .env("RUST_BACKTRACE", "0")
        .output()
        .unwrap();
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "the namespace run failed:\n{text}");
    assert!(
        text.contains("1 passed"),
        "the inner test did not run:\n{text}"
    );
}
