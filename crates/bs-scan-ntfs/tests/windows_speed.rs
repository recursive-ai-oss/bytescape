#![cfg(windows)]
//! How fast the reader is, against the walker, on the same NTFS volume.
//!
//! A virtual disk is filled with `BENCH_FILES` small files (default 200,000)
//! spread over directories, then both tools scan it, alternately, `BENCH_RUNS`
//! times each (default 7).
//!
//! * **cold** means the virtual disk was detached and attached again just
//!   before the scan, so the volume is a new instance and the file system has
//!   nothing cached for it. The virtual disk's own file stays in the host's
//!   page cache, so reads below the file system run at memory speed; this is
//!   cold for NTFS, not for a physical disk.
//! * **warm** means the same tool ran once immediately before.
//!
//! Skipped (and says so) unless `BYTESCAPE_DISPOSABLE_HOST=1` and
//! `BYTESCAPE_NTFS_BENCH=1`. Results are printed and, if `BENCH_OUT` names a
//! file, written there as Markdown.
mod common;

use bs_scan::{ScanOptions, Walker, start};
use bs_scan_ntfs::{Accounting, NtfsOptions, NtfsScanner};
use common::windows::*;
use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn powershell(script: &str) -> String {
    let out = Command::new("powershell")
        .args(["-NoProfile", "-Command", script])
        .output();
    out.map_or_else(
        |e| format!("({e})"),
        |o| String::from_utf8_lossy(&o.stdout).trim().to_string(),
    )
}

fn populate_many(root: &Path, files: u64) {
    let dirs = (files / 100).max(1);
    let threads = 8u64;
    std::thread::scope(|scope| {
        for t in 0..threads {
            scope.spawn(move || {
                let mut d = t;
                while d < dirs {
                    let dir = verbatim(root, &format!("set/d{d:05}"));
                    std::fs::create_dir_all(&dir).expect("dir");
                    for f in 0..(files / dirs) {
                        let i = d * (files / dirs) + f;
                        let size = if i.is_multiple_of(10) {
                            0
                        } else {
                            (i * 37) % 9000 + 1
                        };
                        std::fs::write(
                            dir.join(format!("f{f:03}.dat")),
                            vec![(i % 251) as u8; size as usize],
                        )
                        .expect("file");
                    }
                    d += threads;
                }
            });
        }
    });
}

fn stats(mut v: Vec<f64>) -> (f64, f64, f64) {
    v.sort_by(|a, b| a.partial_cmp(b).expect("numbers"));
    (v[v.len() / 2], v[0], v[v.len() - 1])
}

fn time_walker(root: &Path) -> (Duration, u64, u64) {
    let begun = Instant::now();
    let r = Walker::scan_blocking(root, ScanOptions::default());
    let t = begun.elapsed();
    let n = r.snapshot.root().expect("root");
    (t, n.allocated(), r.snapshot.len() as u64)
}

fn time_reader(raw: &Path, root: &Path) -> (Duration, u64, u64) {
    let scanner = Arc::new(NtfsScanner::new(
        raw,
        NtfsOptions {
            accounting: Accounting::Listing,
            ..NtfsOptions::default()
        },
    ));
    let begun = Instant::now();
    let r = start(scanner, root.to_path_buf(), ScanOptions::default()).wait();
    let t = begun.elapsed();
    assert!(r.stats.error.is_none(), "{:?}", r.stats.error);
    let n = r.snapshot.root().expect("root");
    (t, n.allocated(), r.snapshot.len() as u64)
}

#[test]
fn reader_against_walker() {
    if !allowed("reader_against_walker") {
        return;
    }
    if std::env::var_os("BYTESCAPE_NTFS_BENCH").is_none() {
        println!("NOT RUN: reader_against_walker needs BYTESCAPE_NTFS_BENCH=1");
        return;
    }
    let files = env_u64("BENCH_FILES", 200_000);
    let runs = env_u64("BENCH_RUNS", 7) as usize;
    let mut vhd = Vhd::create("speed", 4096, None);
    let root = vhd.root();
    let made = Instant::now();
    populate_many(&root, files);
    let make_time = made.elapsed();
    vhd.flush();

    let mut out = String::new();
    let host = powershell(
        "$c=Get-CimInstance Win32_Processor | Select-Object -First 1; $o=Get-CimInstance Win32_OperatingSystem; \
         '{0} | {1} cores / {2} threads | {3:N1} GB RAM | {4} {5}' -f $c.Name,$c.NumberOfCores,$c.NumberOfLogicalProcessors,($o.TotalVisibleMemorySize/1MB),$o.Caption,$o.BuildNumber",
    );
    let disk = powershell(
        "Get-PhysicalDisk | ForEach-Object { '{0} ({1}, {2})' -f $_.FriendlyName,$_.MediaType,$_.BusType }",
    );
    let _ = writeln!(out, "host: {host}");
    let _ = writeln!(out, "physical disk(s): {disk}");
    let _ = writeln!(
        out,
        "volume: NTFS on a virtual disk file, {} files made in {:.0?}",
        files, make_time
    );
    let _ = writeln!(out, "runs: {runs} per tool, interleaved");

    let canon = std::fs::canonicalize(&root).expect("canonical");
    let raw = vhd.raw();
    let (mut wc, mut rc, mut ww, mut rw) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut sizes = (0, 0, 0, 0);
    for _ in 0..runs {
        vhd.detach();
        vhd.attach();
        let (t, a, n) = time_walker(&canon);
        wc.push(t.as_secs_f64() * 1e3);
        let (t, _, _) = time_walker(&canon);
        ww.push(t.as_secs_f64() * 1e3);
        sizes.0 = a;
        sizes.1 = n;
        vhd.detach();
        vhd.attach();
        let (t, a, n) = time_reader(&raw, &canon);
        rc.push(t.as_secs_f64() * 1e3);
        let (t, _, _) = time_reader(&raw, &canon);
        rw.push(t.as_secs_f64() * 1e3);
        sizes.2 = a;
        sizes.3 = n;
    }
    let _ = writeln!(
        out,
        "walker: {} allocated bytes, {} nodes; reader: {} allocated bytes, {} nodes",
        sizes.0, sizes.1, sizes.2, sizes.3
    );
    let _ = writeln!(out, "\n| case | median ms | min | max |\n|---|---|---|---|");
    for (name, v) in [
        ("walker cold", wc),
        ("reader cold", rc),
        ("walker warm", ww),
        ("reader warm", rw),
    ] {
        let (m, lo, hi) = stats(v);
        let _ = writeln!(out, "| {name} | {m:.0} | {lo:.0} | {hi:.0} |");
    }
    // The system drive, which is large and cannot be detached: warm only.
    let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    let sys_root = std::fs::canonicalize(format!("{drive}\\")).expect("system drive");
    let sys_raw = std::path::PathBuf::from(format!(r"\\.\{drive}"));
    let (mut sw, mut sr) = (Vec::new(), Vec::new());
    let mut shape = (0, 0, 0, 0);
    time_walker(&sys_root);
    time_reader(&sys_raw, &sys_root);
    for _ in 0..runs.min(3) {
        let (t, a, n) = time_walker(&sys_root);
        sw.push(t.as_secs_f64() * 1e3);
        shape.0 = a;
        shape.1 = n;
        let (t, a, n) = time_reader(&sys_raw, &sys_root);
        sr.push(t.as_secs_f64() * 1e3);
        shape.2 = a;
        shape.3 = n;
    }
    let _ = writeln!(
        out,
        "\nsystem drive {drive} (live, warm after one priming run of each): walker {} allocated bytes, {} nodes; reader {} allocated bytes, {} nodes",
        shape.0, shape.1, shape.2, shape.3
    );
    let _ = writeln!(out, "\n| case | median ms | min | max |\n|---|---|---|---|");
    for (name, v) in [("walker, system drive", sw), ("reader, system drive", sr)] {
        let (m, lo, hi) = stats(v);
        let _ = writeln!(out, "| {name} | {m:.0} | {lo:.0} | {hi:.0} |");
    }
    println!("{out}");
    if let Some(path) = std::env::var_os("BENCH_OUT") {
        std::fs::write(path, &out).expect("write results");
    }
}
