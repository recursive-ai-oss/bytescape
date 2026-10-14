//! Layout timing: a view with about 20,000 visible boxes, a refresh of it,
//! and one directory with a million children.
//!
//! Run with `cargo bench -p bs-layout --bench layout`. Set
//! `BS_LAYOUT_BENCH_OUT` to a file to also write the results there.

#[path = "../tests/common/mod.rs"]
mod common;

use std::fmt::Write as _;
use std::time::Instant;

use bs_layout::{Metrics, Rect, ViewTarget, layout};
use bs_tree::Snapshot;
use common::{Rng, Spec, build, id, synthetic};

fn command(cmd: &str, args: &[&str]) -> String {
    std::process::Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map_or_else(|| "unknown".into(), |s| s.trim().to_string())
}

/// A hash of the source files that were measured (this crate and the size
/// tree it reads), so a result can be tied to exact bytes even when the
/// working tree has uncommitted changes. The bench runs in the crate's
/// directory.
fn source_fingerprint() -> String {
    let script = "find src benches tests Cargo.toml ../bs-tree/src ../bs-tree/Cargo.toml \
         -type f | LC_ALL=C sort | tee /dev/stderr | xargs sha256sum | sha256sum | cut -d' ' -f1";
    let out = std::process::Command::new("sh")
        .args(["-c", script])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let hash = String::from_utf8_lossy(&o.stdout).trim().to_string();
            let files = String::from_utf8_lossy(&o.stderr).lines().count();
            format!(
                "source hash: sha256 {hash} over {files} files (bs-layout src, benches, tests, Cargo.toml; bs-tree src, Cargo.toml)"
            )
        }
        _ => "source hash: unavailable".to_string(),
    }
}

fn read(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

fn method_strip() -> String {
    let cpuinfo = read("/proc/cpuinfo");
    let model = cpuinfo
        .lines()
        .find(|l| l.starts_with("model name"))
        .and_then(|l| l.split(':').nth(1))
        .map_or("unknown", str::trim)
        .to_string();
    let cores = cpuinfo
        .lines()
        .filter(|l| l.starts_with("processor"))
        .count();
    let mem_kb: u64 = read("/proc/meminfo")
        .lines()
        .find(|l| l.starts_with("MemTotal"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut s = String::new();
    let _ = writeln!(
        s,
        "date: {}",
        command("date", &["-u", "+%Y-%m-%dT%H:%M:%SZ"])
    );
    let _ = writeln!(
        s,
        "commit: {} (working tree {})",
        command("git", &["rev-parse", "--short", "HEAD"]),
        if command("git", &["status", "--porcelain", "--", "."]).is_empty() {
            "clean"
        } else {
            "has uncommitted changes under crates/bs-layout"
        }
    );
    let _ = writeln!(s, "{}", source_fingerprint());
    let _ = writeln!(s, "cpu: {model}, {cores} threads");
    let _ = writeln!(s, "ram: {} GiB", mem_kb / 1024 / 1024);
    let _ = writeln!(s, "storage: none (all data in memory)");
    let _ = writeln!(s, "kernel: {}", command("uname", &["-sr"]));
    let _ = writeln!(s, "rustc: {}", command("rustc", &["-V"]));
    let _ = writeln!(s, "profile: release (bench), lto=fat, codegen-units=1");
    let _ = writeln!(s, "threads used by layout: 1");
    let _ = writeln!(s, "load average at start: {}", read("/proc/loadavg").trim());
    let _ = writeln!(
        s,
        "cache state: warm (tree built in memory, {} warm-up runs before timing)",
        WARMUP
    );
    s
}

const WARMUP: usize = 3;

struct Stats {
    median: f64,
    min: f64,
    max: f64,
    runs: usize,
}

fn time<T>(runs: usize, mut f: impl FnMut() -> T) -> Stats {
    for _ in 0..WARMUP {
        std::hint::black_box(f());
    }
    let mut times: Vec<f64> = (0..runs)
        .map(|_| {
            let t = Instant::now();
            std::hint::black_box(f());
            t.elapsed().as_secs_f64() * 1000.0
        })
        .collect();
    times.sort_by(f64::total_cmp);
    Stats {
        median: times[times.len() / 2],
        min: times[0],
        max: times[times.len() - 1],
        runs,
    }
}

fn row(out: &mut String, what: &str, st: &Stats) {
    let _ = writeln!(
        out,
        "{what}: median {:.2} ms, min {:.2} ms, max {:.2} ms (N={})",
        st.median, st.min, st.max, st.runs
    );
}

fn dataset(spec: &Spec, snap: &Snapshot) -> String {
    let files = spec.nodes.iter().filter(|n| !n.is_dir).count();
    let dirs = spec.nodes.len() - files;
    let bytes = snap.root().map_or(0, |r| r.allocated());
    format!(
        "{files} files, {dirs} directories, {} allocated bytes",
        bytes
    )
}

fn main() {
    let mut out = String::new();
    out.push_str("bs-layout benchmark\n");
    out.push_str(&method_strip());

    // One synthetic disk, laid out in a 4K window at several font sizes. The
    // number of boxes a window can show is limited by the thresholds, which
    // are in em: the smaller the font, the more boxes fit.
    let viewport = Rect::new(0.0, 0.0, 3840.0, 2160.0);
    let spec = synthetic(1_200_000, 20_240_101);
    let snap = build(&spec);
    let _ = writeln!(
        out,
        "\ndataset A (generated by the bench, seed 20240101): {}",
        dataset(&spec, &snap)
    );
    let _ = writeln!(out, "window: {}x{} device px", viewport.w, viewport.h);
    let target = ViewTarget::Folder(id(0));
    for (what, font, scale) in [
        ("font 13 px at 150% (em 19.5)", 13.0f32, 1.5f32),
        ("font 13 px at 100% (em 13)", 13.0, 1.0),
        ("font 8 px at 100% (em 8)", 8.0, 1.0),
        ("font 5 px at 100% (em 5)", 5.0, 1.0),
        ("font 4 px at 100% (em 4)", 4.0, 1.0),
        ("font 3 px at 100% (em 3)", 3.0, 1.0),
    ] {
        let m = Metrics::from_logical(font, scale);
        let st = time(31, || layout(&snap, &target, viewport, m));
        let mut l = layout(&snap, &target, viewport, m);
        let groups = l
            .items()
            .iter()
            .filter(|i| i.kind == bs_layout::ItemKind::Group)
            .count();
        let _ = writeln!(
            out,
            "\n{what}: {} boxes ({groups} groups, {} enterable)",
            l.items().len(),
            l.enterable().len()
        );
        row(&mut out, "  layout", &st);
        let st = time(31, || l.refresh(&snap));
        row(&mut out, "  refresh", &st);
    }

    // One directory with a million children.
    let mut rng = Rng(7);
    let mut big = Spec::new();
    for _ in 0..1_000_000 {
        let size = ((rng.unit() * 11.0).exp() as u64).max(1) * 512;
        big.file(0, size);
    }
    let snap = build(&big);
    let hd = Rect::new(0.0, 0.0, 1920.0, 1080.0);
    let m1 = Metrics::from_logical(13.0, 1.0);
    let _ = writeln!(
        out,
        "\ndataset B (one directory, generated by the bench, seed 7): {}",
        dataset(&big, &snap)
    );
    let folder = ViewTarget::Folder(id(0));
    let st = time(9, || layout(&snap, &folder, hd, m1));
    let l = layout(&snap, &folder, hd, m1);
    let _ = writeln!(
        out,
        "view {}x{}, em {}: boxes drawn {}, enterable {}",
        hd.w,
        hd.h,
        m1.em,
        l.items().len(),
        l.enterable().len()
    );
    row(&mut out, "layout of the folder view", &st);
    if let Some((_, group)) = l
        .enterable()
        .iter()
        .find(|(_, t)| matches!(t, ViewTarget::Group(_)))
    {
        let st = time(9, || layout(&snap, group, hd, m1));
        let inside = layout(&snap, group, hd, m1);
        let _ = writeln!(
            out,
            "inside the group: boxes drawn {}, truncated {}",
            inside.items().len(),
            inside.is_truncated()
        );
        row(&mut out, "layout of the group view", &st);
    }

    print!("{out}");
    if let Ok(path) = std::env::var("BS_LAYOUT_BENCH_OUT") {
        std::fs::write(path, &out).expect("could not write the results file");
    }
}
