//! Benchmark for the io_uring `statx` experiment: the walker next to the same
//! walker with its per-entry `statx` calls batched through a ring, run one
//! after the other, run by run, so that drift on a shared machine affects
//! them alike.
//!
//! ```text
//! uring_bench [--runs N] [--threads 4,16] [--sink drop|tree] [--depths 32,128,512]
//!             [--async off|on|both] [--no-prime] [--cold] [--tsv FILE] PATH...
//! uring_bench --make DIR --entries N
//! ```
//!
//! * Each variant is timed around the whole scan (wall clock) and the CPU time
//!   the process used (user plus system, which includes the kernel's worker
//!   threads that serve the ring's requests).
//! * `--sink drop` times the walker alone with its batches discarded;
//!   `--sink tree` times the whole pipeline with the tree applier.
//! * One untimed run of each variant first (unless `--no-prime`) so the
//!   measurement is warm. Nothing empties the file cache unless `--cold` is
//!   given: then, before every timed run, the program syncs and writes `3` to
//!   `/proc/sys/vm/drop_caches`. That needs root and
//!   `BYTESCAPE_DISPOSABLE_HOST=1`, and the program exits with failure rather
//!   than quietly measure warm if it cannot.
//! * Every variant must report the same totals as the plain walker, or the
//!   run is marked as a mismatch and the process exits with failure.
//! * `--make DIR --entries N` builds a tree of about N entries, all empty
//!   files, so a million-entry dataset costs inodes and directory blocks
//!   but almost no data. The same arguments make the same tree.
//!
//! The summary goes to standard output as Markdown tables; `--tsv FILE` also
//! writes every timed run.

#[cfg(target_os = "linux")]
mod imp {
    use bs_scan::iouring_bench::{Config, IoUringWalker, probe, process_cpu_ms, take_stats};
    use bs_scan::{ScanContext, ScanOptions, Scanner, Walker, start};
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Instant;

    #[derive(Clone, Copy, Debug)]
    enum Variant {
        Plain,
        Ring(Config),
    }

    impl Variant {
        fn label(self) -> String {
            match self {
                Variant::Plain => "walker (getdents64 + statx)".to_string(),
                Variant::Ring(c) => format!(
                    "ring depth {}{}",
                    c.depth,
                    if c.force_async { " +IOSQE_ASYNC" } else { "" }
                ),
            }
        }

        fn scanner(self) -> Arc<dyn Scanner> {
            match self {
                Variant::Plain => Arc::new(Walker),
                Variant::Ring(c) => Arc::new(IoUringWalker::new(c)),
            }
        }
    }

    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    struct Totals {
        allocated: u64,
        logical: u64,
        files: u64,
        dirs: u64,
        unreadable: u64,
        vanished: u64,
    }

    struct Sample {
        wall_ms: f64,
        cpu_ms: f64,
        totals: Totals,
        requests: u64,
        enters: u64,
    }

    /// Empties the page, dentry and inode caches. Exits when that is not
    /// possible.
    fn drop_caches() {
        let _ = std::process::Command::new("sync").status();
        if let Err(e) = std::fs::write("/proc/sys/vm/drop_caches", b"3") {
            eprintln!("--cold needs root to write /proc/sys/vm/drop_caches: {e}");
            std::process::exit(3);
        }
    }

    fn one_run(variant: Variant, path: &Path, threads: usize, sink: &str, cold: bool) -> Sample {
        if cold {
            drop_caches();
        }
        let options = ScanOptions {
            threads: Some(threads),
            ..ScanOptions::default()
        };
        let _ = take_stats();
        let cpu0 = process_cpu_ms();
        let begun = Instant::now();
        let (totals, wall_ms, cpu) = match sink {
            "tree" => {
                let result = start(variant.scanner(), path.to_path_buf(), options).wait();
                let wall = begun.elapsed().as_secs_f64() * 1000.0;
                let cpu = process_cpu_ms() - cpu0;
                if let Some(e) = &result.stats.error {
                    eprintln!("scan failed: {e}");
                    std::process::exit(1);
                }
                let p = result.stats.progress;
                (
                    Totals {
                        allocated: p.allocated_bytes,
                        logical: p.logical_bytes,
                        files: p.files,
                        dirs: p.dirs,
                        unreadable: p.unreadable,
                        vanished: p.vanished,
                    },
                    wall,
                    cpu,
                )
            }
            _ => {
                let (ctx, rx) = ScanContext::channel(options, 64);
                let drain = std::thread::spawn(move || rx.iter().count());
                let scanner = variant.scanner();
                let end = scanner.scan(path, &ctx);
                let p = ctx.progress();
                drop(ctx);
                let _ = drain.join();
                let wall = begun.elapsed().as_secs_f64() * 1000.0;
                let cpu = process_cpu_ms() - cpu0;
                if let Err(e) = end {
                    eprintln!("scan failed: {e}");
                    std::process::exit(1);
                }
                (
                    Totals {
                        allocated: p.allocated_bytes,
                        logical: p.logical_bytes,
                        files: p.files,
                        dirs: p.dirs,
                        unreadable: p.unreadable,
                        vanished: p.vanished,
                    },
                    wall,
                    cpu,
                )
            }
        };
        let stats = take_stats();
        Sample {
            wall_ms,
            cpu_ms: cpu,
            totals,
            requests: stats.requests,
            enters: stats.enters,
        }
    }

    fn median(v: &mut [f64]) -> f64 {
        v.sort_by(|a, b| a.total_cmp(b));
        let n = v.len();
        if n == 0 {
            0.0
        } else if n % 2 == 1 {
            v[n / 2]
        } else {
            (v[n / 2 - 1] + v[n / 2]) / 2.0
        }
    }

    fn list(arg: &str) -> Vec<u32> {
        arg.split(',')
            .filter_map(|v| v.trim().parse().ok())
            .collect()
    }

    // ---- dataset generator ---------------------------------------------------

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
    }

    fn make(dir: &Path, entries: u64) {
        let marker = dir.join(".uring-bench-tree");
        let want = format!("entries={entries}\n");
        if std::fs::read_to_string(&marker).is_ok_and(|m| m == want) {
            println!("reused=1 entries={entries}");
            return;
        }
        std::fs::create_dir_all(dir).expect("create the dataset directory");
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let mut made = 0u64;
        let mut dirs = 0u64;
        // Breadth-first: each directory gets 20..180 files and up to 12
        // subdirectories, down to depth 5.
        let mut level: Vec<PathBuf> = vec![dir.to_path_buf()];
        for depth in 0..6 {
            let mut next = Vec::new();
            for d in &level {
                let files = 20 + rng.next() % 160;
                for i in 0..files {
                    if made >= entries {
                        break;
                    }
                    let name = format!("f{:06x}-{i}.dat", rng.next() % 0xff_ffff);
                    if std::fs::File::create(d.join(name)).is_ok() {
                        made += 1;
                    }
                }
                if depth < 5 {
                    for i in 0..(rng.next() % 13) {
                        if made >= entries {
                            break;
                        }
                        let sub = d.join(format!("d{i}"));
                        if std::fs::create_dir(&sub).is_ok() {
                            made += 1;
                            dirs += 1;
                            next.push(sub);
                        }
                    }
                }
            }
            if made >= entries || next.is_empty() {
                break;
            }
            level = next;
        }
        // Top up with files in the root if the shape ran out early.
        let mut i = 0u64;
        while made < entries {
            if std::fs::File::create(dir.join(format!("top-{i}.dat"))).is_ok() {
                made += 1;
            }
            i += 1;
        }
        std::fs::write(&marker, want).expect("write the marker");
        println!("reused=0 entries={made} dirs={dirs}");
    }

    pub fn run() {
        let mut runs = 10usize;
        let mut threads = vec![4u32, 16];
        let mut sink = "drop".to_string();
        let mut depths = vec![32u32, 128, 512];
        let mut asyncs = "both".to_string();
        let mut prime = true;
        let mut cold = false;
        let mut tsv: Option<PathBuf> = None;
        let mut paths = Vec::new();
        let mut make_dir: Option<PathBuf> = None;
        let mut entries = 1_000_000u64;
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--runs" => runs = args.next().and_then(|v| v.parse().ok()).unwrap_or(runs),
                "--threads" => threads = list(&args.next().unwrap_or_default()),
                "--sink" => sink = args.next().unwrap_or_default(),
                "--depths" => depths = list(&args.next().unwrap_or_default()),
                "--async" => asyncs = args.next().unwrap_or_default(),
                "--no-prime" => prime = false,
                "--cold" => {
                    cold = true;
                    prime = false;
                }
                "--tsv" => tsv = args.next().map(PathBuf::from),
                "--make" => make_dir = args.next().map(PathBuf::from),
                "--entries" => {
                    entries = args.next().and_then(|v| v.parse().ok()).unwrap_or(entries)
                }
                other if !other.starts_with("--") => paths.push(PathBuf::from(other)),
                other => {
                    eprintln!("unknown option {other}");
                    std::process::exit(2);
                }
            }
        }
        if let Some(dir) = make_dir {
            make(&dir, entries);
            return;
        }
        if paths.is_empty() || threads.is_empty() {
            eprintln!(
                "usage: uring_bench [--runs N] [--threads 4,16] [--sink drop|tree] [--depths 32,128,512] [--async off|on|both] [--no-prime] [--cold] [--tsv FILE] PATH..."
            );
            std::process::exit(2);
        }
        if cold && std::env::var("BYTESCAPE_DISPOSABLE_HOST").as_deref() != Ok("1") {
            eprintln!("--cold needs BYTESCAPE_DISPOSABLE_HOST=1");
            std::process::exit(3);
        }
        if let Err(e) = probe() {
            eprintln!("io_uring statx is not available here: {e:?}");
            std::process::exit(3);
        }

        let mut variants = vec![Variant::Plain];
        for &depth in &depths {
            for force_async in [false, true] {
                let wanted = match asyncs.as_str() {
                    "off" => !force_async,
                    "on" => force_async,
                    _ => true,
                };
                if wanted {
                    variants.push(Variant::Ring(Config { depth, force_async }));
                }
            }
        }

        let mut tsv_out = tsv.map(|p| std::fs::File::create(p).expect("create the tsv file"));
        if let Some(f) = tsv_out.as_mut() {
            writeln!(f, "dataset\tthreads\tvariant\trun\twall_ms\tcpu_ms\tallocated\tfiles\tdirs\trequests\tenters").ok();
        }
        let load_before = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
        let mut mismatch = false;
        println!(
            "# io_uring statx experiment ({sink} sink, {runs} runs, interleaved, {} cache)\n",
            if cold { "COLD" } else { "warm" }
        );
        for path in &paths {
            for &t in &threads {
                let t = t as usize;
                if prime {
                    for v in &variants {
                        let _ = one_run(*v, path, t, &sink, cold);
                    }
                }
                let mut walls: Vec<Vec<f64>> = vec![Vec::new(); variants.len()];
                let mut cpus: Vec<Vec<f64>> = vec![Vec::new(); variants.len()];
                let mut reqs = vec![0u64; variants.len()];
                let mut enters = vec![0u64; variants.len()];
                let mut reference: Option<Totals> = None;
                let mut bad = vec![false; variants.len()];
                for run in 1..=runs {
                    for (i, v) in variants.iter().enumerate() {
                        let s = one_run(*v, path, t, &sink, cold);
                        walls[i].push(s.wall_ms);
                        cpus[i].push(s.cpu_ms);
                        reqs[i] = s.requests;
                        enters[i] = s.enters;
                        match reference {
                            None => reference = Some(s.totals),
                            Some(r) if r != s.totals => {
                                bad[i] = true;
                                mismatch = true;
                            }
                            Some(_) => {}
                        }
                        if let Some(f) = tsv_out.as_mut() {
                            writeln!(
                                f,
                                "{}\t{}\t{}\t{}\t{:.3}\t{:.3}\t{}\t{}\t{}\t{}\t{}",
                                path.display(),
                                t,
                                v.label(),
                                run,
                                s.wall_ms,
                                s.cpu_ms,
                                s.totals.allocated,
                                s.totals.files,
                                s.totals.dirs,
                                s.requests,
                                s.enters
                            )
                            .ok();
                        }
                    }
                }
                let r = reference.unwrap_or_default();
                println!(
                    "## {} — {} threads — {} entries, {} dirs, {} allocated bytes\n",
                    path.display(),
                    t,
                    r.files + r.dirs,
                    r.dirs,
                    r.allocated
                );
                println!(
                    "| variant | wall median ms | min | max | vs walker | CPU median ms | requests | enter calls | totals |"
                );
                println!("|---|---|---|---|---|---|---|---|---|");
                let base = median(&mut walls[0].clone());
                for (i, v) in variants.iter().enumerate() {
                    let mut w = walls[i].clone();
                    let med = median(&mut w);
                    let min = w.first().copied().unwrap_or(0.0);
                    let max = w.last().copied().unwrap_or(0.0);
                    let mut c = cpus[i].clone();
                    println!(
                        "| {} | {:.1} | {:.1} | {:.1} | {:+.1}% | {:.0} | {} | {} | {} |",
                        v.label(),
                        med,
                        min,
                        max,
                        (med - base) / base * 100.0,
                        median(&mut c),
                        reqs[i],
                        enters[i],
                        if bad[i] { "MISMATCH" } else { "equal" }
                    );
                }
                println!();
            }
        }
        let load_after = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
        println!(
            "Host load average before: {}; after: {}",
            load_before
                .split_whitespace()
                .take(3)
                .collect::<Vec<_>>()
                .join(" "),
            load_after
                .split_whitespace()
                .take(3)
                .collect::<Vec<_>>()
                .join(" ")
        );
        if mismatch {
            eprintln!(
                "FAILED: a variant's totals differ from the walker's; the timings are not a comparison"
            );
            std::process::exit(1);
        }
    }
}

fn main() {
    #[cfg(target_os = "linux")]
    imp::run();
}
