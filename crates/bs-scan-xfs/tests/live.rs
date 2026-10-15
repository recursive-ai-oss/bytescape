//! The XFS reader against the directory walker, on a real XFS file system:
//! the same fixture is scanned by both and compared path by path (type,
//! logical size, allocated size, link identity, modification time). The
//! exceptions table is empty: the bulk read reports the inode's own block
//! count, which is what `statx` reports for a file whose data has been
//! written out, so any difference is a finding.
//!
//! It mounts a file system, so it needs root, and it is declared with
//!
//! ```text
//! BYTESCAPE_DISPOSABLE_HOST=1 cargo test -p bs-scan-xfs --test live -- --nocapture
//! ```
//!
//! With the variable unset every test returns at once and prints that it did
//! not run. With it set, a test that cannot do its job (no root, no `mount`,
//! no `mkfs.xfs`) fails instead of passing quietly.
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

#[path = "common/diff.rs"]
mod diff;

use bs_scan::{ScanOptions, ScanResult, Walker, start};
use bs_scan_xfs::{XfsOptions, XfsScanner};
use bs_tree::NodeFlags;
use diff::{compare, tree_of};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

fn enabled(test: &str) -> bool {
    if std::env::var("BYTESCAPE_DISPOSABLE_HOST").as_deref() != Ok("1") {
        eprintln!("NOT RUN: {test} needs BYTESCAPE_DISPOSABLE_HOST=1 and root");
        return false;
    }
    let uid = Command::new("id").arg("-u").output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&uid.stdout).trim(),
        "0",
        "{test}: BYTESCAPE_DISPOSABLE_HOST=1 was given but this is not root"
    );
    true
}

/// Runs a shell command and fails the test with its output when it fails.
fn sh(script: &str) {
    let out = Command::new("bash")
        .args(["-ec", script])
        .output()
        .unwrap_or_else(|e| panic!("cannot run bash: {e}"));
    assert!(
        out.status.success(),
        "`{script}` failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Mounted loop file systems, unmounted and removed when dropped.
struct Fs {
    dir: PathBuf,
    mnt: PathBuf,
}

impl Fs {
    fn new(name: &str) -> Fs {
        let dir = std::env::temp_dir().join(format!("bs-xfs-live-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mnt = dir.join("mnt");
        std::fs::create_dir_all(&mnt).unwrap();
        let img = dir.join("fs.img");
        sh(&format!(
            "truncate -s 700M '{img}' && mkfs.xfs -q -f '{img}' && mount -o loop '{img}' '{mnt}'",
            img = img.display(),
            mnt = mnt.display(),
        ));
        Fs { dir, mnt }
    }

    fn sync(&self) {
        sh("sync");
    }
}

impl Drop for Fs {
    fn drop(&mut self) {
        let _ = Command::new("umount").arg(self.mnt.join("other")).status();
        let _ = Command::new("umount").arg(&self.mnt).status();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Everything the reader describes that an XFS file system can hold.
fn build_fixture(fs: &Fs) {
    let m = fs.mnt.display();
    let d = fs.dir.display();
    sh(&format!(
        r#"cd '{m}'
mkdir -p docs deep/a/b/c/d empty-dir big-dir other
for i in $(seq 1 3000); do : > big-dir/f$i; done
for i in $(seq 1 20); do head -c $((i * 20)) /dev/urandom > docs/tiny-$i; done
head -c 6000 /dev/urandom > docs/readme.txt
head -c 8388608 /dev/urandom > big.bin
cp --reflink=always big.bin clone-a.bin
cp --reflink=always big.bin clone-b.bin
truncate -s 64M sparse.bin
dd if=/dev/urandom of=sparse.bin bs=4096 count=1 seek=256 conv=notrunc status=none
fallocate -l 2M prealloc.bin
ln docs/readme.txt hard-readme
ln docs/tiny-3 docs/tiny-3-link
ln -s docs/readme.txt link
ln -s /nonexistent/target dangling
mkfifo fifo
printf x > "$(printf 'odd-\377-name\nwith-newline')"
head -c 300000 /dev/urandom > deep/a/b/c/d/leaf.bin
# Another file system mounted inside this one: not entered by either tool.
truncate -s 300M '{d}/other.img'
mkfs.xfs -q -f '{d}/other.img'
mount -o loop '{d}/other.img' '{m}/other'
head -c 100000 /dev/urandom > '{m}/other/not-ours.bin'
"#
    ));
}

fn walk(mnt: &Path) -> ScanResult {
    Walker::scan_blocking(mnt, ScanOptions::default())
}

fn read(mnt: &Path) -> (ScanResult, bs_scan_xfs::XfsReport) {
    let scanner = Arc::new(XfsScanner::new(XfsOptions::default()));
    let result = start(scanner.clone(), mnt.to_path_buf(), ScanOptions::default()).wait();
    assert!(result.stats.error.is_none(), "{:?}", result.stats.error);
    let report = scanner.last_report().expect("a report");
    (result, report)
}

#[test]
fn the_reader_matches_the_walker_on_an_xfs_file_system() {
    if !enabled("the_reader_matches_the_walker_on_an_xfs_file_system") {
        return;
    }
    let fs = Fs::new("diff");
    build_fixture(&fs);
    fs.sync();
    let walker = walk(&fs.mnt);
    assert!(walker.snapshot.scan_complete());
    let (reader, report) = read(&fs.mnt);
    assert!(reader.snapshot.scan_complete(), "{:?}", report.diagnostics);
    let w = tree_of(&walker.snapshot);
    let r = tree_of(&reader.snapshot);
    let outcome = compare(&w, &r, &[]);
    let text = outcome.render();
    println!("{text}");
    if let Ok(dir) = std::env::var("BENCH_RESULTS") {
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(Path::new(&dir).join("xfs-differential.txt"), &text).unwrap();
        std::fs::write(
            Path::new(&dir).join("xfs-report.txt"),
            format!("{report:#?}\n"),
        )
        .unwrap();
    }
    assert!(outcome.is_clean(), "{text}");
    assert_eq!(report.diagnostics.total(), 0, "{:?}", report.diagnostics);

    // What the fixture is meant to show.
    assert!(w["other"].flags.contains(NodeFlags::NOT_FOLLOWED));
    assert!(r["other"].flags.contains(NodeFlags::NOT_FOLLOWED));
    assert!(!r.contains_key("other/not-ours.bin"));
    assert!(r.keys().filter(|k| k.starts_with("big-dir/")).count() == 3000);
    assert!(r["hard-readme"].flags.contains(NodeFlags::HARD_LINK));
    assert!(
        r["sparse.bin"]
            .flags
            .contains(NodeFlags::SPARSE_OR_COMPRESSED)
    );
    assert!(r["prealloc.bin"].allocated >= 2 << 20);
    // Entries made after the bulk read are looked up one by one; none here.
    assert_eq!(report.looked_up, 0, "{report:?}");
    // The whole tree adds up.
    assert_eq!(
        walker.snapshot.root().unwrap().allocated(),
        reader.snapshot.root().unwrap().allocated()
    );
}

#[test]
fn files_made_while_the_scan_runs_are_looked_up_or_counted_as_vanished() {
    if !enabled("files_made_while_the_scan_runs_are_looked_up_or_counted_as_vanished") {
        return;
    }
    let fs = Fs::new("live");
    sh(&format!(
        "cd '{m}'; mkdir d; for i in $(seq 1 2000); do head -c 8192 /dev/urandom > d/f$i; done; sync",
        m = fs.mnt.display()
    ));
    // Churn: make and remove files while the reader runs.
    let mnt = fs.mnt.clone();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let churn = std::thread::spawn(move || {
        let mut n = 0u64;
        while !flag.load(std::sync::atomic::Ordering::Relaxed) {
            let p = mnt.join(format!("d/new{n}"));
            let _ = std::fs::write(&p, vec![1u8; 4096]);
            if n >= 50 {
                let _ = std::fs::remove_file(mnt.join(format!("d/new{}", n - 50)));
            }
            n += 1;
        }
    });
    let mut runs = 0;
    for _ in 0..10 {
        let (result, report) = read(&fs.mnt);
        result.snapshot.verify().unwrap();
        // However the churn fell, nothing is lost silently: whatever was
        // dropped is counted and marks its directory.
        assert_eq!(result.stats.records_rejected, 0);
        let root = result.snapshot.root().unwrap();
        if report.unreadable_entries > 0 {
            assert!(root.bytes_are_lower_bound());
        }
        runs += 1;
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    churn.join().unwrap();
    assert_eq!(runs, 10);
}
