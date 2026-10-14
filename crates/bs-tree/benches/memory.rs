//! Memory and publication-cost benchmark for the size tree.
//!
//! Run with `cargo bench -p bs-tree --bench memory`. Options, after `--`:
//!
//! * `--memory-sizes 1000000,5000000` node counts for the bytes-per-node runs
//! * `--publish-sizes 1000000,10000000` node counts for the publication runs
//! * `--cycles 15` timed cycles per publication scenario
//! * `--out FILE` also write the report to FILE
//!
//! Memory is measured with a counting global allocator: live heap bytes
//! before and after building the tree, not `size_of` arithmetic. Names are
//! separated by building the same tree twice, once with real names and once
//! with empty ones.
//!
//! The figure that matters is what a program holds after the scan: the
//! aggregator kept alive for delete marks, after `finish()`, with a published
//! snapshot. That is measured with and without a hard-link key on every file
//! (what a scanner on a volume with no link counts has to send). The
//! working set during the scan is reported beside it: the live bytes sampled
//! after every batch, and the operating system's peak resident set from a
//! fresh child process (`--child`).

use bs_tree::{Aggregator, Batch, DirKey, FileKey, Meta, NodeKind, Snapshot};
use stats_alloc::{INSTRUMENTED_SYSTEM, StatsAlloc};
use std::alloc::System;
use std::fmt::Write as _;
use std::process::Command;
use std::time::{Duration, Instant};

#[global_allocator]
static GLOBAL: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

/// Heap bytes currently allocated by the whole process.
fn live_bytes() -> i64 {
    let s = INSTRUMENTED_SYSTEM.stats();
    s.bytes_allocated as i64 - s.bytes_deallocated as i64
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const BATCH_RECORDS: usize = 4096;
/// Directories deeper than this get no subdirectories, so paths stay
/// realistic (real trees are rarely deeper than a few dozen levels).
const MAX_DEPTH: u32 = 16;
const EXTENSIONS: [&[u8]; 8] = [
    b".txt", b".js", b".rs", b".png", b".json", b".so", b".md", b".o",
];
const NAME_CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789_-";

/// Deterministic scan-like workload: a depth-first walk where each directory
/// is listed in one go (about 12 entries, one in nine a subdirectory), the
/// way a walker produces records.
struct Workload {
    shape: Rng,
    names: Rng,
    /// Directories still to list: key and depth.
    stack: Vec<(u32, u32)>,
    next_key: u32,
    count: u64,
    target: u64,
    empty_names: bool,
    /// Give every file a hard-link key.
    keyed: bool,
    next_file_id: u128,
    leave_open: bool,
    started: bool,
    root_closed: bool,
}

impl Workload {
    fn new(target: u64, empty_names: bool, keyed: bool, leave_open: bool) -> Workload {
        Workload {
            shape: Rng(0x9E37_79B9_7F4A_7C15),
            names: Rng(0xD1B5_4A32_D192_ED03),
            stack: Vec::new(),
            next_key: 1,
            count: 0,
            target,
            empty_names,
            keyed,
            next_file_id: 0,
            leave_open,
            started: false,
            root_closed: false,
        }
    }

    fn name(&mut self, is_dir: bool, out: &mut Vec<u8>) {
        out.clear();
        if self.empty_names {
            return;
        }
        // Triangular length distribution: files about 4..30 bytes with an
        // extension most of the time, directories 3..16.
        let len = if is_dir {
            3 + (self.names.below(7) + self.names.below(7)) as usize
        } else {
            3 + (self.names.below(12) + self.names.below(12)) as usize
        };
        for _ in 0..len {
            out.push(NAME_CHARS[self.names.below(NAME_CHARS.len() as u64) as usize]);
        }
        if !is_dir && self.names.below(10) < 8 {
            out.extend_from_slice(EXTENSIONS[self.names.below(8) as usize]);
        }
    }

    fn meta(&mut self, is_dir: bool) -> Meta {
        if is_dir {
            return Meta {
                allocated: 4096,
                mtime: 1_700_000_000 + self.shape.below(50_000_000) as i64,
                ..Meta::default()
            };
        }
        let logical = (1u64 << self.shape.below(24)) + self.shape.below(1000);
        let link = self.keyed.then(|| {
            self.next_file_id += 1;
            FileKey {
                volume: 1,
                id: self.next_file_id,
            }
        });
        Meta {
            logical,
            allocated: logical.div_ceil(4096) * 4096,
            mtime: 1_700_000_000 + self.shape.below(50_000_000) as i64,
            link,
            ..Meta::default()
        }
    }

    /// Adds records to `batch` until it holds about `BATCH_RECORDS`. Returns
    /// whether there is more to produce.
    fn fill(&mut self, batch: &mut Batch) -> bool {
        let mut name = Vec::new();
        if !self.started {
            self.started = true;
            self.name(true, &mut name);
            let root = self.meta(true);
            batch.dir(DirKey(0), DirKey::NONE, b"/data", &root).unwrap();
            self.count = 1;
        }
        while batch.len() < BATCH_RECORDS {
            let Some((dir, depth)) = self.stack.pop() else {
                if self.count < self.target {
                    // Every subtree so far has ended; start another one
                    // under the root, which stays open until the end.
                    self.name(true, &mut name);
                    let meta = self.meta(true);
                    let key = self.next_key;
                    self.next_key += 1;
                    batch.dir(DirKey(key), DirKey(0), &name, &meta).unwrap();
                    self.stack.push((key, 1));
                    self.count += 1;
                    continue;
                }
                if !self.leave_open && !self.root_closed {
                    batch.listing_done(DirKey(0));
                    self.root_closed = true;
                }
                break;
            };
            let remaining = self.target.saturating_sub(self.count);
            let children = if remaining == 0 || self.shape.below(20) == 0 {
                0
            } else {
                (1 + self.shape.below(24)).min(remaining)
            };
            for _ in 0..children {
                let is_dir = depth < MAX_DEPTH && self.shape.below(9) == 0;
                self.name(is_dir, &mut name);
                let meta = self.meta(is_dir);
                if is_dir {
                    let key = self.next_key;
                    self.next_key += 1;
                    batch.dir(DirKey(key), DirKey(dir), &name, &meta).unwrap();
                    self.stack.push((key, depth + 1));
                } else {
                    batch
                        .file(NodeKind::File, DirKey(dir), &name, &meta)
                        .unwrap();
                }
                self.count += 1;
            }
            if !self.leave_open {
                batch.listing_done(DirKey(dir));
            }
        }
        self.count < self.target || !self.stack.is_empty()
    }
}

struct Built {
    agg: Aggregator,
    ingest: Duration,
    /// Highest live heap size seen after any batch, relative to the start.
    peak_live: i64,
}

fn build(target: u64, empty_names: bool, keyed: bool, leave_open: bool) -> Built {
    let before = live_bytes();
    let mut agg = Aggregator::new();
    let mut work = Workload::new(target, empty_names, keyed, leave_open);
    let mut batch = Batch::new();
    let mut peak_live = 0;
    let start = Instant::now();
    loop {
        batch.clear();
        let more = work.fill(&mut batch);
        let report = agg.apply(&batch);
        assert_eq!(report.rejected, 0, "workload must be valid");
        peak_live = peak_live.max(live_bytes() - before);
        if !more {
            break;
        }
    }
    let ingest = start.elapsed();
    Built {
        agg,
        ingest,
        peak_live,
    }
}

struct MemoryRow {
    nodes: u32,
    dirs: u32,
    ingest: Duration,
    /// Live bytes with the aggregator still holding scan-time state.
    scanning: i64,
    /// Highest live bytes sampled after a batch.
    peak_live: i64,
    /// Aggregator alive after `finish()`, one published snapshot alive.
    held: i64,
    /// Only a snapshot alive (aggregator dropped).
    snapshot_only: i64,
    name_bytes_used: usize,
}

/// Builds the tree and measures what is held at each stage of a scan.
fn measure_memory(target: u64, empty_names: bool, keyed: bool) -> MemoryRow {
    let before = live_bytes();
    let Built {
        mut agg,
        ingest,
        peak_live,
    } = build(target, empty_names, keyed, false);
    let scanning = live_bytes() - before;
    agg.finish();
    let snap = agg.publish();
    assert!(snap.scan_complete());
    let held = live_bytes() - before;
    let dirs = count_dirs(&snap);
    let nodes = snap.len();
    let name_bytes_used = snap.name_bytes();
    drop(agg);
    let snapshot_only = live_bytes() - before;
    drop(snap);
    MemoryRow {
        nodes,
        dirs,
        ingest,
        scanning,
        peak_live,
        held,
        snapshot_only,
        name_bytes_used,
    }
}

/// Runs a scan in this process and prints the process's baseline and peak
/// resident set (kB), for the parent to read. Used through `--child`.
fn child(target: u64, keyed: bool) {
    let baseline = read_first("/proc/self/status", "VmHWM");
    let Built { mut agg, .. } = build(target, false, keyed, false);
    agg.finish();
    let _held = agg.publish();
    let peak = read_first("/proc/self/status", "VmHWM");
    println!("{baseline}|{peak}");
}

fn kb(text: &str) -> f64 {
    text.split_whitespace()
        .next()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(f64::NAN)
}

/// Peak resident set of a fresh process that scans `target` nodes.
fn peak_rss(target: u64, keyed: bool) -> (f64, f64) {
    let exe = std::env::current_exe().expect("own path");
    let out = Command::new(exe)
        .args([
            "--child",
            &target.to_string(),
            if keyed { "1" } else { "0" },
        ])
        .output()
        .expect("run child");
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().last().unwrap_or("");
    let (base, peak) = line.split_once('|').unwrap_or(("nan", "nan"));
    (kb(base), kb(peak))
}

fn count_dirs(snap: &Snapshot) -> u32 {
    (0..snap.len())
        .filter(|&i| {
            snap.node(bs_tree::NodeId::from_index(i))
                .is_some_and(|n| n.kind() == NodeKind::Dir)
        })
        .count() as u32
}

fn summarize(mut times: Vec<Duration>) -> String {
    times.sort();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    format!(
        "median {:.3} ms (min {:.3}, max {:.3}, n={})",
        ms(times[times.len() / 2]),
        ms(times[0]),
        ms(times[times.len() - 1]),
        times.len()
    )
}

/// One scenario of changes between publications: `dirs` directories get
/// `per_dir` new files each.
fn change_batch(agg_keys: u32, dirs: u32, per_dir: u32, round: u64) -> Batch {
    let mut rng = Rng(0xABCDEF ^ round.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut batch = Batch::new();
    for d in 0..dirs {
        let dir = if dirs == 1 {
            agg_keys - 1
        } else {
            rng.below(u64::from(agg_keys)) as u32
        };
        for f in 0..per_dir {
            let name = format!("new-{round}-{d}-{f}.dat");
            let meta = Meta {
                logical: 1000,
                allocated: 4096,
                ..Meta::default()
            };
            batch
                .file(NodeKind::File, DirKey(dir), name.as_bytes(), &meta)
                .unwrap();
        }
    }
    batch
}

fn measure_publish(out: &mut String, target: u64, cycles: usize) {
    let before = live_bytes();
    let Built {
        mut agg, ingest, ..
    } = build(target, false, false, true);
    let built = live_bytes() - before;
    let keys = count_keys(&mut agg);
    let _ = writeln!(
        out,
        "\n### Publication at {} nodes (all directories left open; build took {:.2} s, {:.0} k records/s with no publishing)\n",
        target,
        ingest.as_secs_f64(),
        target as f64 / ingest.as_secs_f64() / 1000.0
    );
    let _ = writeln!(
        out,
        "Tree plus applier state after build: {:.1} MiB.\n",
        built as f64 / (1 << 20) as f64
    );

    // Idle publication: nothing changed since the last one.
    agg.publish();
    let mut idle = Vec::new();
    for _ in 0..cycles + 2 {
        let t = Instant::now();
        let s = agg.publish();
        idle.push(t.elapsed());
        drop(s);
    }
    idle.drain(..2);
    let _ = writeln!(out, "- publish, nothing changed: {}", summarize(idle));

    for (label, dirs, per_dir) in [
        (
            "concentrated: 4096 new files in one directory",
            1u32,
            4096u32,
        ),
        (
            "scattered: 64 files in each of 64 random directories",
            64,
            64,
        ),
    ] {
        let mut apply_t = Vec::new();
        let mut publish_t = Vec::new();
        let mut cow = Vec::new();
        for round in 0..(cycles + 2) as u64 {
            let batch = change_batch(keys, dirs, per_dir, round);
            // A reader is holding the last published snapshot, so the first
            // write to each chunk copies it.
            let held = agg.publish();
            let mem = live_bytes();
            let t = Instant::now();
            let r = agg.apply(&batch);
            let a = t.elapsed();
            assert_eq!(r.rejected, 0);
            let extra = live_bytes() - mem;
            let t = Instant::now();
            let next = agg.publish();
            let p = t.elapsed();
            drop(held);
            drop(next);
            if round >= 2 {
                apply_t.push(a);
                publish_t.push(p);
                cow.push(extra);
            }
        }
        cow.sort();
        let _ = writeln!(
            out,
            "- {label}:\n  - apply while the previous snapshot is held (copy-on-write included): {}\n  - publish: {}\n  - extra heap while both versions are alive: median {:.2} MiB (min {:.2}, max {:.2})",
            summarize(apply_t),
            summarize(publish_t),
            cow[cow.len() / 2] as f64 / (1 << 20) as f64,
            cow[0] as f64 / (1 << 20) as f64,
            cow[cow.len() - 1] as f64 / (1 << 20) as f64,
        );
    }
}

/// Number of directory keys in the workload tree (every key is a directory).
fn count_keys(agg: &mut Aggregator) -> u32 {
    let snap = agg.snapshot();
    count_dirs(&snap)
}

fn read_first(path: &str, key: &str) -> String {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| {
            t.lines()
                .find(|l| l.starts_with(key))
                .map(|l| l.split_once(':').map_or(l, |x| x.1).trim().to_string())
        })
        .unwrap_or_else(|| "unknown".into())
}

fn command(cmd: &str, args: &[&str]) -> String {
    std::process::Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

fn method_strip(out: &mut String) {
    let cpus = std::thread::available_parallelism().map_or(0, |n| n.get());
    let commit = command("git", &["rev-parse", "--short", "HEAD"]);
    let dirty = command("git", &["status", "--porcelain", "--", "."]);
    let _ = writeln!(out, "## Method\n");
    let _ = writeln!(
        out,
        "- date: {}",
        command("date", &["-u", "+%Y-%m-%dT%H:%M:%SZ"])
    );
    let _ = writeln!(
        out,
        "- commit: {commit}{}",
        if dirty == "unknown" || dirty.is_empty() {
            ""
        } else {
            " + uncommitted bs-tree changes"
        }
    );
    let _ = writeln!(
        out,
        "- CPU: {} ({} hardware threads)",
        read_first("/proc/cpuinfo", "model name"),
        cpus
    );
    let _ = writeln!(out, "- RAM: {}", read_first("/proc/meminfo", "MemTotal"));
    let _ = writeln!(
        out,
        "- storage / filesystem: not used (everything is in memory)"
    );
    let _ = writeln!(out, "- kernel: {}", command("uname", &["-sr"]));
    let _ = writeln!(out, "- rustc: {}", command("rustc", &["--version"]));
    let _ = writeln!(
        out,
        "- build profile: {}",
        if cfg!(debug_assertions) {
            "debug (do not quote these numbers)"
        } else {
            "bench (release, fat LTO, codegen-units=1)"
        }
    );
    let _ = writeln!(out, "- threads: 1 (applier) for every measurement");
    let _ = writeln!(
        out,
        "- host load average at start: {}",
        read_first("/proc/loadavg", "")
            .split(' ')
            .take(3)
            .collect::<Vec<_>>()
            .join(" ")
    );
    let _ = writeln!(
        out,
        "- cache state: warm (all data is freshly built in process memory; no disk I/O)"
    );
    let _ = writeln!(
        out,
        "- host is shared with other work, so timings are noisy; byte counts are exact"
    );
    let _ = writeln!(
        out,
        "- allocator accounting: `stats_alloc` counting wrapper around the system allocator; live bytes = bytes allocated - bytes released, reallocation growth included"
    );
    let _ = writeln!(
        out,
        "- dataset: generated, deterministic (fixed seeds); depth-first walk, 1..24 entries per directory (5% empty), one entry in nine is a directory except below depth 16; file names 4..30 bytes (+ extension 80% of the time), directory names 3..16 bytes, raw ASCII; sizes log-uniform up to 16 MiB; records applied in batches of about {BATCH_RECORDS}"
    );
}

fn main() {
    let mut memory_sizes = vec![1_000_000u64, 5_000_000];
    let mut publish_sizes = vec![1_000_000u64, 10_000_000];
    let mut cycles = 15usize;
    let mut out_path: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let parse = |v: Option<String>| -> Vec<u64> {
            v.unwrap_or_default()
                .split(',')
                .filter_map(|s| s.parse().ok())
                .collect()
        };
        match arg.as_str() {
            "--memory-sizes" => memory_sizes = parse(args.next()),
            "--publish-sizes" => publish_sizes = parse(args.next()),
            "--cycles" => cycles = args.next().and_then(|v| v.parse().ok()).unwrap_or(cycles),
            "--out" => out_path = args.next(),
            "--child" => {
                let target = args.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                let keyed = args.next().as_deref() == Some("1");
                child(target, keyed);
                return;
            }
            _ => {}
        }
    }

    let mut out = String::new();
    let _ = writeln!(out, "# bs-tree memory and publication benchmark\n");
    method_strip(&mut out);

    let _ = writeln!(
        out,
        "\n## Memory per node, as held after the scan (allocator accounting)\n"
    );
    let _ = writeln!(
        out,
        "State measured: the aggregator kept alive after `finish()` (as a program that offers Undo holds it) with one published snapshot. \"link keys\" says whether every file carried a hard-link key.\n"
    );
    let _ = writeln!(
        out,
        "| nodes | dirs | link keys | held B/node | structure B/node (names empty) | names B/node (difference) | pool bytes used B/node (counter) | snapshot alone B/node |"
    );
    let _ = writeln!(out, "|---|---|---|---|---|---|---|---|");
    let mut scan_rows = String::new();
    for &size in &memory_sizes {
        for keyed in [false, true] {
            let with_names = measure_memory(size, false, keyed);
            let without = measure_memory(size, true, keyed);
            let n = f64::from(with_names.nodes);
            assert_eq!(with_names.nodes, without.nodes);
            let _ = writeln!(
                out,
                "| {} | {} | {} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} |",
                with_names.nodes,
                with_names.dirs,
                if keyed { "every file" } else { "none" },
                with_names.held as f64 / n,
                without.held as f64 / n,
                (with_names.held - without.held) as f64 / n,
                with_names.name_bytes_used as f64 / n,
                with_names.snapshot_only as f64 / n,
            );
            let (base_kb, peak_kb) = peak_rss(size, keyed);
            let _ = writeln!(
                scan_rows,
                "| {} | {} | {:.2} | {:.2} | {:.2} | {:.1} | {:.2} | {:.2} s ({:.0} k records/s) |",
                with_names.nodes,
                if keyed { "every file" } else { "none" },
                with_names.scanning as f64 / n,
                with_names.peak_live as f64 / n,
                with_names.held as f64 / n,
                (peak_kb - base_kb) / 1024.0,
                (peak_kb - base_kb) * 1024.0 / n,
                with_names.ingest.as_secs_f64(),
                n / with_names.ingest.as_secs_f64() / 1000.0,
            );
        }
    }
    let _ = writeln!(
        out,
        "\n\"structure\" is the same tree built with every name empty, so it includes the one-byte name length prefix each node keeps in the pool (1 B/node). \"names\" is the extra memory real names add, prefix growth included. \"snapshot alone\" is what remains if the aggregator is dropped, which gives up delete marks."
    );

    let _ = writeln!(out, "\n## Working set during the scan\n");
    let _ = writeln!(
        out,
        "| nodes | link keys | live at end of scan, before `finish()` B/node | peak live after any batch B/node | held after `finish()` B/node | peak resident set above start, MiB | peak resident set above start B/node | ingest |"
    );
    let _ = writeln!(out, "|---|---|---|---|---|---|---|---|");
    out.push_str(&scan_rows);
    let _ = writeln!(
        out,
        "\nBefore `finish()` the aggregator keeps a map from directory key to node, the directories still open, and (with keys) a map from file identity to the first node that had it; `finish()` frees all of that. \"Peak live\" is sampled after each batch, so it misses a table doubling inside a batch. The resident-set peak is the operating system's high-water mark for a separate process that does only this scan (`/proc/self/status` VmHWM, minus its value at start); it includes allocator slack and the batch buffers and is the figure to use for the working set."
    );

    let _ = writeln!(out, "\n## Publication cost");
    for &size in &publish_sizes {
        measure_publish(&mut out, size, cycles);
    }

    print!("{out}");
    if let Some(path) = out_path {
        std::fs::write(&path, &out).expect("write report");
    }
}
