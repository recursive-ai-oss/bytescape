//! What a hostile volume can make the reader allocate, against the memory
//! limit the caller sets. A counting allocator sees every thread's
//! allocations during a read (the reader's, the tree applier's, everything
//! the scan holds), and the peak must stay within the limit plus a stated
//! allowance; a volume that needs more is refused.

mod common;
#[path = "common/hostile.rs"]
mod hostile;

use bs_scan_ntfs::NtfsOptions;
use common::testkit::{NonRes, ROOT, RecordBuilder, Spec, Volume, encode_runs};
use hostile::*;
use std::sync::{Arc, Mutex};

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// One read at a time: the allocator counts the whole process.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

/// What a read may take beyond its limit: the chunks the parsing threads
/// hold between two checks, the growth of the vectors that hold the
/// results, the tree's own tables. At the
/// limits tested here (a few megabytes) that is a few megabytes.
const ALLOWANCE: usize = 2 << 20;

/// What each stage's refusal says: the table is parsed, its names are joined,
/// and the directories and the tree are made.
const STAGES: [&str; 4] = [
    "the table needs more memory",
    "names and attributes need more memory",
    "joining the table's names",
    "the directories and the tree",
];

fn cjk_name(i: usize, len: usize) -> String {
    let mut s: String = std::iter::repeat_n('\u{4E00}', len - 4).collect();
    s.push_str(&format!("{i:04}"));
    s
}

fn read(img: &[u8], cap: u64, threads: usize) -> Outcome {
    let src = Patched::clean(Arc::new(img.to_vec()));
    run_variant_with(
        src,
        NtfsOptions {
            max_memory_bytes: cap,
            threads: Some(threads),
            ..NtfsOptions::default()
        },
        u64::MAX,
    )
}

/// Thousands of long names on few records: far more heap than the records
/// are big.
fn names_volume(long: bool) -> Vec<u8> {
    let mut v = Volume::new(Spec {
        record_bytes: 4096,
        cluster_bytes: 4096,
        clusters: 4200,
        mft_area_clusters: 4000,
        ..Spec::default()
    });
    for i in 0..3000usize {
        let len = if long { 255 } else { 8 };
        let f = v.file(&cjk_name(i, len), ROOT, 0);
        for k in 0..5 {
            v.link(f, &cjk_name(i * 10 + k + 100_000, len), ROOT);
        }
    }
    v.finish()
}

#[test]
fn the_peak_stays_within_the_limit_however_many_names_a_volume_has() {
    let _one = ONE_AT_A_TIME.lock().unwrap();
    for long in [false, true] {
        let img = names_volume(long);
        let mut accepted = Vec::new();
        let mut refused = 0;
        // From far too little to plenty.
        let mut cap = 512u64 << 10;
        while cap <= 256 << 20 {
            for threads in [1, 4] {
                let out = read(&img, cap, threads);
                assert!(out.panicked.is_none() && !out.timed_out);
                // Refused or not, the read stayed within its limit: a volume
                // that is turned away late has already cost memory.
                assert!(
                    out.peak as u64 <= cap + ALLOWANCE as u64,
                    "long={long} cap={cap} threads={threads}: peak {} B is {:.1}x the limit",
                    out.peak,
                    out.peak as f64 / cap as f64
                );
                if out.refusal.is_some() {
                    let why = out.refusal.as_deref().unwrap();
                    assert!(why.contains("memory"), "cap {cap}: {why}");
                    refused += 1;
                } else {
                    accepted.push((cap, out.peak));
                }
            }
            cap *= 2;
        }
        // The ladder must have both sides, or it proves nothing.
        assert!(
            refused > 0 && !accepted.is_empty(),
            "long={long}: {refused} refused, {accepted:?}"
        );
        let (smallest, peak) = accepted[0];
        println!(
            "long={long}: refused {refused}, smallest accepted limit {smallest} B, its peak {peak} B"
        );
        // Names are most of the bytes, so the smallest limit that works is
        // not small.
        assert!(smallest >= (img.len() as u64) / 64, "{smallest}");
    }
}

/// A table whose runs repeat, which made a small image read as a huge one.
#[test]
fn a_table_that_maps_its_clusters_again_and_again_is_refused() {
    let _one = ONE_AT_A_TIME.lock().unwrap();
    let (rb, cb) = (4096u64, 4096u64);
    let mut v = Volume::new(Spec {
        record_bytes: 4096,
        cluster_bytes: 4096,
        clusters: 200,
        mft_area_clusters: 160,
        ..Spec::default()
    });
    for i in 0..120 {
        let f = v.file(&cjk_name(i, 255), ROOT, 0);
        for k in 0..5 {
            v.link(f, &cjk_name(1000 + i * 10 + k, 255), ROOT);
        }
    }
    let mut img = v.finish();
    let top = 24 + 120u64;
    let tc = (top * rb).div_ceil(cb);
    let copies = 900u64;
    let runs: Vec<(Option<u64>, u64)> = (0..copies).map(|_| (Some(4), tc)).collect();
    let size = copies * tc * cb;
    let rec0 = RecordBuilder::new(rb as usize, 1)
        .number(0)
        .standard_info(0x6, 1_700_000_000)
        .file_name(5, 5, 3, "$MFT")
        .nonres(&NonRes {
            ty: 0x80,
            name: String::new(),
            flags: 0,
            lowest_vcn: 0,
            highest_vcn: copies * tc - 1,
            unit: 0,
            allocated: size,
            size,
            initialized: size,
            compressed: None,
            runs: encode_runs(&runs),
        })
        .build();
    let at = (4 * cb) as usize;
    img[at..at + rb as usize].copy_from_slice(&rec0);
    let cap = 40u64 << 20;
    let out = read(&img, cap, 4);
    let why = out.refusal.expect("refused");
    assert!(why.contains("overlap"), "{why}");
    assert!(out.peak < 4 << 20, "{}", out.peak);
}

/// A limit too small for even the file table is refused before anything is
/// read.
#[test]
fn a_limit_smaller_than_the_file_table_refuses_at_once() {
    let _one = ONE_AT_A_TIME.lock().unwrap();
    let img = names_volume(false);
    let out = read(&img, 64 << 10, 4);
    assert!(out.refusal.expect("refused").contains("memory"));
    assert!(out.peak < 1 << 20);
}

/// Many records, each with a short name: the memory is in the file table, the
/// directory index and the tree, which the read charges as it goes.
#[test]
fn a_volume_of_many_small_files_stays_within_the_limit_too() {
    let _one = ONE_AT_A_TIME.lock().unwrap();
    let mut v = Volume::new(Spec {
        record_bytes: 512,
        cluster_bytes: 4096,
        clusters: 5_400,
        mft_area_clusters: 5_200,
        ..Spec::default()
    });
    for i in 0..40_000usize {
        v.file(&format!("f{i}"), ROOT, 0);
    }
    let img = v.finish();
    let (mut refused, mut accepted) = (0, 0);
    let mut stages = std::collections::BTreeSet::new();
    let mut cap = 2u64 << 20;
    while cap <= 40 << 20 {
        for threads in [1, 4] {
            let out = read(&img, cap, threads);
            assert!(out.panicked.is_none() && !out.timed_out);
            assert!(
                out.peak as u64 <= cap + ALLOWANCE as u64,
                "cap={cap} threads={threads}: peak {} B is {:.2}x the limit",
                out.peak,
                out.peak as f64 / cap as f64
            );
            println!(
                "{} files, limit {cap}, {threads} threads: peak {} B, {}",
                40_000,
                out.peak,
                out.refusal.as_deref().unwrap_or("read")
            );
            if let Some(why) = &out.refusal {
                refused += 1;
                for stage in STAGES {
                    if why.contains(stage) {
                        stages.insert(stage);
                    }
                }
            } else {
                accepted += 1;
            }
        }
        cap = cap * 5 / 4;
    }
    assert!(
        refused > 0 && accepted > 0,
        "{refused} refused, {accepted} accepted"
    );
    // Every place the limit is enforced turned a read away somewhere on the
    // ladder, so none of them is dead.
    assert_eq!(stages.len(), STAGES.len(), "stages seen: {stages:?}");
}
