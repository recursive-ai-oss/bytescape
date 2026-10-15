//! Cost of listing the large folders of a snapshot, and of replacing one
//! folder's contents, on a tree of a million nodes or more.
//!
//! Run with `cargo bench -p bs-tree --bench large_dirs`. Options, after `--`:
//!
//! * `--nodes 1000000,5000000` tree sizes
//! * `--runs 15` timed runs per row (after one untimed run)
//! * `--out FILE` also write the report to FILE
//!
//! The tree is built in memory from a seeded generator, so there is no disk
//! I/O and nothing to drop from a cache.

use bs_tree::{Aggregator, Batch, DirKey, Meta, NodeId, NodeKind, Snapshot};
use std::fmt::Write as _;
use std::time::{Duration, Instant};

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

const MAX_DEPTH: u32 = 16;

struct Entry {
    parent: u32,
    is_dir: bool,
    logical: u64,
    allocated: u64,
    children: Vec<u32>,
}

/// Entry 0 is the root. Every directory lists 1..24 entries (5% none), one in
/// nine of them a directory except below `MAX_DEPTH`; file sizes are
/// log-uniform up to 16 MiB.
fn generate(target: u64) -> Vec<Entry> {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut entries = vec![Entry {
        parent: 0,
        is_dir: true,
        logical: 0,
        allocated: 4096,
        children: Vec::new(),
    }];
    let mut queue: std::collections::VecDeque<(u32, u32)> = std::collections::VecDeque::new();
    queue.push_back((0, 0));
    while (entries.len() as u64) < target {
        let Some((dir, depth)) = queue.pop_front() else {
            // Every subtree ended early: start another one under the root.
            let id = entries.len() as u32;
            entries.push(Entry {
                parent: 0,
                is_dir: true,
                logical: 0,
                allocated: 4096,
                children: Vec::new(),
            });
            entries[0].children.push(id);
            queue.push_back((id, 1));
            continue;
        };
        let count = if rng.below(20) == 0 {
            0
        } else {
            1 + rng.below(24)
        };
        for _ in 0..count {
            if entries.len() as u64 >= target {
                break;
            }
            let is_dir = depth < MAX_DEPTH && rng.below(9) == 0;
            let id = entries.len() as u32;
            let (logical, allocated) = if is_dir {
                (0, 4096)
            } else {
                let l = (1u64 << rng.below(24)) + rng.below(1000);
                (l, l.div_ceil(4096) * 4096)
            };
            entries.push(Entry {
                parent: dir,
                is_dir,
                logical,
                allocated,
                children: Vec::new(),
            });
            if let Some(parent) = entries.get_mut(dir as usize) {
                parent.children.push(id);
            }
            if is_dir {
                queue.push_back((id, depth + 1));
            }
        }
    }
    entries
}

fn name_of(id: u32) -> Vec<u8> {
    format!("n{id}").into_bytes()
}

/// The records of directory `dir` and everything below it, parents first. For
/// the root record `root_name` is used.
fn subtree_batches(entries: &[Entry], dir: u32, root_name: &[u8]) -> Vec<Batch> {
    let mut out = Vec::new();
    let mut batch = Batch::new();
    let meta = |e: &Entry| Meta {
        logical: e.logical,
        allocated: e.allocated,
        ..Meta::default()
    };
    batch
        .dir(
            DirKey(dir),
            DirKey::NONE,
            root_name,
            &meta(&entries[dir as usize]),
        )
        .unwrap();
    let mut stack = vec![dir];
    while let Some(d) = stack.pop() {
        for &c in &entries[d as usize].children {
            let e = &entries[c as usize];
            if e.is_dir {
                batch
                    .dir(DirKey(c), DirKey(d), &name_of(c), &meta(e))
                    .unwrap();
                stack.push(c);
            } else {
                batch
                    .file(NodeKind::File, DirKey(d), &name_of(c), &meta(e))
                    .unwrap();
            }
        }
        batch.listing_done(DirKey(d));
        if batch.len() > 4000 {
            out.push(std::mem::take(&mut batch));
        }
    }
    out.push(batch);
    out
}

fn build(entries: &[Entry]) -> Aggregator {
    let mut agg = Aggregator::new();
    for batch in subtree_batches(entries, 0, b"/data") {
        assert_eq!(agg.apply(&batch).rejected, 0);
    }
    agg.finish();
    agg.publish();
    agg
}

/// Every directory below the root with at least `min` allocated bytes within
/// `depth` levels, found by looking at every node: the answer without the
/// pruning.
fn full_walk(snap: &Snapshot, min: u64, depth: u32, limit: usize) -> usize {
    let mut found: Vec<(u64, u32)> = Vec::new();
    let mut stack = vec![(snap.root().unwrap().id(), 0u32)];
    while let Some((id, d)) = stack.pop() {
        let Some(node) = snap.node(id) else { continue };
        for child in node.children() {
            if child.kind() != NodeKind::Dir {
                continue;
            }
            if d < depth && child.allocated() >= min {
                found.push((child.allocated(), child.id().index()));
            }
            // No pruning: every directory is entered.
            if d + 1 < depth {
                stack.push((child.id(), d + 1));
            }
        }
    }
    found.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    found.truncate(limit);
    found.len()
}

fn summarize(mut times: Vec<Duration>) -> String {
    times.sort();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    format!(
        "{:.3} ms (min {:.3}, max {:.3}, n={})",
        ms(times[times.len() / 2]),
        ms(times[0]),
        ms(times[times.len() - 1]),
        times.len()
    )
}

fn time<T>(runs: usize, mut f: impl FnMut() -> T) -> (String, T) {
    let mut last = f();
    let mut times = Vec::new();
    for _ in 0..runs {
        let start = Instant::now();
        last = std::hint::black_box(f());
        times.push(start.elapsed());
    }
    (summarize(times), last)
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

fn method_strip(out: &mut String, runs: usize) {
    let cpus = std::thread::available_parallelism().map_or(0, |n| n.get());
    let dirty = command("git", &["status", "--porcelain", "--", "."]);
    let _ = writeln!(out, "## Method\n");
    let _ = writeln!(
        out,
        "- date: {}",
        command("date", &["-u", "+%Y-%m-%dT%H:%M:%SZ"])
    );
    let _ = writeln!(
        out,
        "- commit: {}{}",
        command("git", &["rev-parse", "--short", "HEAD"]),
        if dirty == "unknown" || dirty.is_empty() {
            ""
        } else {
            " + uncommitted changes"
        }
    );
    let _ = writeln!(
        out,
        "- CPU: {} ({cpus} hardware threads)",
        read_first("/proc/cpuinfo", "model name")
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
            "bench (release)"
        }
    );
    let _ = writeln!(out, "- threads: 1");
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
        "- cache state: warm (the tree is freshly built in process memory; one untimed run before the {runs} timed runs)"
    );
    let _ = writeln!(
        out,
        "- host is shared with other work, so timings are noisy"
    );
    let _ = writeln!(
        out,
        "- dataset: generated, deterministic (fixed seed); breadth-first, 1..24 entries per directory (5% empty), one entry in nine a directory except below depth {MAX_DEPTH}; file sizes log-uniform up to 16 MiB"
    );
    let _ = writeln!(
        out,
        "- protocol: median of the timed runs, with min and max\n"
    );
}

fn main() {
    let mut sizes = vec![1_000_000u64, 5_000_000];
    let mut runs = 15usize;
    let mut out_path: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--nodes" => {
                sizes = args
                    .next()
                    .unwrap_or_default()
                    .split(',')
                    .filter_map(|s| s.parse().ok())
                    .collect();
            }
            "--runs" => runs = args.next().and_then(|v| v.parse().ok()).unwrap_or(runs),
            "--out" => out_path = args.next(),
            _ => {}
        }
    }
    let mut out = String::new();
    let _ = writeln!(out, "# bs-tree large-folder query and rescan benchmark\n");
    method_strip(&mut out, runs);

    let _ = writeln!(
        out,
        "## Large folders: pruned query against a walk of every node\n"
    );
    let _ = writeln!(
        out,
        "| nodes | dirs | min size | depth | limit | found | pruned query | walk of every node |"
    );
    let _ = writeln!(out, "|---|---|---|---|---|---|---|---|");
    let mut rescan_rows = String::new();
    for &target in &sizes {
        let entries = generate(target);
        let mut agg = build(&entries);
        let snap = agg.publish();
        let dirs = entries.iter().filter(|e| e.is_dir).count();
        for &(min, depth, limit) in &[
            (100u64 << 20, 4u32, 200usize),
            (100 << 20, 99, 200),
            (1 << 30, 99, 200),
            (1 << 20, 99, 200),
            (0, 99, 200),
        ] {
            let (pruned, result) = time(runs, || {
                snap.large_dirs(NodeId::ROOT, min, depth, limit).unwrap()
            });
            let (walk, walked) = time(runs, || full_walk(&snap, min, depth, limit));
            assert_eq!(result.dirs.len(), walked, "same answer, min {min}");
            let _ = writeln!(
                out,
                "| {} | {dirs} | {} MiB | {depth} | {limit} | {} | {pruned} | {walk} |",
                entries.len(),
                min >> 20,
                result.dirs.len()
            );
        }

        // Replace the contents of a directory with the same contents again:
        // the directories whose subtrees are closest to 1,000 and to 100,000
        // nodes, and (for scale) a new tree of the same size built from
        // scratch.
        let mut below = vec![1u32; entries.len()];
        for i in (1..entries.len()).rev() {
            let p = entries[i].parent as usize;
            below[p] += below[i];
        }
        for want in [1_000u32, 100_000] {
            let Some((dir, size)) = entries
                .iter()
                .enumerate()
                .filter(|(i, e)| e.is_dir && *i != 0)
                .map(|(i, _)| (i as u32, below[i]))
                .min_by_key(|&(_, n)| n.abs_diff(want))
            else {
                continue;
            };
            let mut begin_times = Vec::new();
            let mut apply_times = Vec::new();
            let mut publish_times = Vec::new();
            let mut scratch_times = Vec::new();
            // Node ids follow the order of the records, not the generator's
            // numbers, so the folder is found by its path.
            let id = {
                let mut chain = vec![dir];
                while let Some(&last) = chain.last() {
                    if last == 0 {
                        break;
                    }
                    chain.push(entries[last as usize].parent);
                }
                let mut at = snap.root().expect("a root");
                for &step in chain.iter().rev().skip(1) {
                    let want = name_of(step);
                    at = at
                        .children()
                        .find(|c| c.name() == want.as_slice())
                        .expect("the folder is in the tree");
                }
                at.id()
            };
            for run in 0..=runs {
                let batches = subtree_batches(&entries, dir, b"x");
                let start = Instant::now();
                agg.begin_rescan(id, None).unwrap();
                let begin = start.elapsed();
                let start = Instant::now();
                for b in &batches {
                    assert_eq!(agg.apply(b).rejected, 0);
                }
                agg.finish();
                let apply = start.elapsed();
                let start = Instant::now();
                agg.publish();
                let publish = start.elapsed();
                let start = Instant::now();
                let mut fresh = Aggregator::new();
                for b in &batches {
                    assert_eq!(fresh.apply(b).rejected, 0);
                }
                fresh.finish();
                let scratch = start.elapsed();
                if run > 0 {
                    begin_times.push(begin);
                    apply_times.push(apply);
                    publish_times.push(publish);
                    scratch_times.push(scratch);
                }
            }
            let _ = writeln!(
                rescan_rows,
                "| {} | {size} | {} | {} | {} | {} |",
                entries.len(),
                summarize(begin_times),
                summarize(apply_times),
                summarize(publish_times),
                summarize(scratch_times)
            );
        }
    }
    let _ = writeln!(
        out,
        "\nThe pruned query enters only directories that qualify; the walk enters all of them. Both return the same directories (asserted)."
    );

    let _ = writeln!(
        out,
        "\n## Replacing a folder's contents with the same contents\n"
    );
    let _ = writeln!(
        out,
        "| tree nodes | nodes in the folder | begin | apply + finish (cut off the old contents, count the new) | publish after | the same folder built as a new tree |"
    );
    let _ = writeln!(out, "|---|---|---|---|---|---|");
    out.push_str(&rescan_rows);
    println!("{out}");

    if let Some(path) = out_path {
        std::fs::write(path, &out).expect("write the report");
    }
}
