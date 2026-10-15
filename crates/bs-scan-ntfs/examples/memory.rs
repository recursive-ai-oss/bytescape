//! Measures what the reader allocates per file record, with a counting
//! allocator, on a volume built in memory: `memory [files]`.
#![allow(unsafe_code)]

use bs_scan::{ScanOptions, start};
#[path = "../tests/common/testkit.rs"]
mod testkit;

use bs_scan_ntfs::{Accounting, NtfsOptions};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use testkit::{ROOT, SourceScanner, Spec, Volume};

struct Counting;
static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// SAFETY: forwards every call to the system allocator with the layout it was
// given; the counters are atomics and allocate nothing.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let now = CURRENT.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
        PEAK.fetch_max(now, Ordering::Relaxed);
        // SAFETY: same layout as the caller's.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: `ptr` and `layout` are the caller's, from this allocator.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new: usize) -> *mut u8 {
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
        let now = CURRENT.fetch_add(new, Ordering::Relaxed) + new;
        PEAK.fetch_max(now, Ordering::Relaxed);
        // SAFETY: `ptr` and `layout` are the caller's, from this allocator.
        unsafe { System.realloc(ptr, layout, new) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn main() {
    let files: u64 = std::env::args()
        .nth(1)
        .and_then(|n| n.parse().ok())
        .unwrap_or(200_000);
    let dirs = (files / 100).max(1);
    let records = files + dirs + 30;
    let clusters = records / 4 + files / 2 + 4096;
    let mut v = Volume::new(Spec {
        clusters: clusters + records / 4 + 64,
        mft_area_clusters: records / 4 + 64,
        ..Spec::default()
    });
    let mut parents = Vec::new();
    for d in 0..dirs {
        parents.push(v.dir(&format!("dir{d:05}"), ROOT));
    }
    for i in 0..files {
        let size = if i % 5 == 0 { 3000 } else { 200 };
        v.file(
            &format!("file-{i:07}.dat"),
            parents[(i % dirs) as usize],
            size,
        );
    }
    let image = v.finish();
    let mut rows = Vec::new();
    for threads in [1usize, 4] {
        let scanner = SourceScanner::new(
            image.clone(),
            NtfsOptions {
                threads: Some(threads),
                accounting: Accounting::Full,
                ..NtfsOptions::default()
            },
        );
        let before = CURRENT.load(Ordering::Relaxed);
        PEAK.store(before, Ordering::Relaxed);
        let begun = std::time::Instant::now();
        let result = start(scanner.clone(), "C:\\".into(), ScanOptions::default()).wait();
        let elapsed = begun.elapsed();
        assert!(result.stats.error.is_none());
        let peak = PEAK.load(Ordering::Relaxed) - before;
        let report = scanner.outcome.lock().unwrap().take();
        let records = report.and_then(Result::ok).map_or(0, |r| r.records);
        rows.push((threads, elapsed, peak, records));
        println!(
            "{threads} thread(s): {:.0} ms, peak {} KiB over the image and baseline ({} bytes per record slot), {} nodes in the tree",
            elapsed.as_secs_f64() * 1e3,
            peak / 1024,
            peak as u64 / records.max(1),
            result.snapshot.len()
        );
    }
}
