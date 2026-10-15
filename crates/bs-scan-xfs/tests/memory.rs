//! What a large file system can make the reader allocate, against the memory
//! limit the caller sets. A counting allocator sees every thread's
//! allocations during a read (the reader's, the walk threads', the tree
//! applier's), and the peak must stay within the limit plus a stated
//! allowance; a file system that needs more is refused.
#![cfg(target_os = "linux")]
#![allow(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

mod common;

use bs_scan::{ScanContext, ScanEnd, ScanError, ScanOptions, Scanner, start};
use bs_scan_xfs::{XfsOptions, read_filesystem};
use common::mem::{MemXfs, ROOT};
use std::alloc::{GlobalAlloc, Layout, System};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

struct Counting;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every method forwards to the system allocator with the arguments it
// was given and only adds bookkeeping, so the contract of `GlobalAlloc` is the
// system allocator's own.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded unchanged.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let now = CURRENT.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged.
        unsafe { System.dealloc(ptr, layout) };
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: forwarded unchanged.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            if new_size >= layout.size() {
                let grown = new_size - layout.size();
                let now = CURRENT.fetch_add(grown, Ordering::Relaxed) + grown;
                PEAK.fetch_max(now, Ordering::Relaxed);
            } else {
                CURRENT.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        p
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// One read at a time: the allocator counts the whole process.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

/// What a read may take beyond its limit: the vectors' growth steps and the
/// tree's own tables. At the limits tested here that is a few megabytes.
const ALLOWANCE: usize = 4 << 20;

struct Shared {
    fs: Arc<MemXfs>,
    options: XfsOptions,
}

impl Scanner for Shared {
    fn name(&self) -> &'static str {
        "mem-xfs"
    }

    fn scan(&self, _root: &Path, ctx: &ScanContext) -> Result<ScanEnd, ScanError> {
        read_filesystem(&*self.fs, &*self.fs, b"/mnt", &self.options, ctx)
            .map(|(end, _)| end)
            .map_err(|e| ScanError::Root(std::io::Error::other(e)))
    }
}

/// The peak above the starting point, and whether the read was refused.
fn measure(fs: &Arc<MemXfs>, cap: u64, threads: usize) -> (usize, bool) {
    let scanner = Arc::new(Shared {
        fs: Arc::clone(fs),
        options: XfsOptions {
            max_memory_bytes: cap,
            threads: Some(threads),
        },
    });
    let base = CURRENT.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let result = start(scanner, "/".into(), ScanOptions::default()).wait();
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(base);
    (peak, matches!(result.stats.error, Some(ScanError::Root(_))))
}

fn big_fs() -> Arc<MemXfs> {
    let mut fs = MemXfs::new();
    let mut n = 0u64;
    for d in 0..300 {
        let dir = fs.mkdir(ROOT, &format!("{d:03}-{}", "d".repeat(200)));
        for f in 0..200 {
            n += 1;
            let name = format!("{f:04}-{n}-{}", "n".repeat(190));
            fs.file(dir, name.as_bytes(), 8192, 2);
        }
    }
    Arc::new(fs)
}

#[test]
fn the_peak_stays_within_the_limit_however_big_the_file_system_is() {
    let _one = ONE_AT_A_TIME.lock().unwrap();
    let fs = big_fs();
    let mut accepted = Vec::new();
    let mut refused = 0;
    let mut cap = 256u64 << 10;
    while cap <= 256 << 20 {
        for threads in [1usize, 4] {
            let (peak, was_refused) = measure(&fs, cap, threads);
            assert!(
                peak as u64 <= cap + ALLOWANCE as u64,
                "cap={cap} threads={threads}: peak {peak} B is {:.1}x the limit (refused: {was_refused})",
                peak as f64 / cap as f64
            );
            if was_refused {
                refused += 1;
            } else {
                accepted.push((cap, peak));
            }
        }
        cap *= 2;
    }
    // The ladder must have both sides, or it proves nothing.
    assert!(
        refused > 0 && !accepted.is_empty(),
        "{refused} refused, {accepted:?}"
    );
    let (smallest, peak) = accepted[0];
    println!("refused {refused}, smallest accepted limit {smallest} B, its peak {peak} B");
}
