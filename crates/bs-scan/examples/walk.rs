//! A command-line front end for the walker, used by the throughput benchmark script and
//! handy for trying a scan by hand.
//!
//! ```text
//! walk [--threads N] [--sink tree|drop] [--drive] PATH
//! ```
//!
//! * `--sink tree` (the default) runs the whole pipeline: walker, applier
//!   thread and the final snapshot, which is what the application does.
//! * `--sink drop` runs only the walker and discards its batches, to show how
//!   fast the directory reads alone go.
//!
//! * `--drive` asks the platform crate which drive holds `PATH`, enters the
//!   other mounts of that drive and measures progress against its used bytes,
//!   the way the application starts a scan.
//!
//! The last line printed is one line of `key=value` pairs.

use bs_scan::{Boundary, ScanContext, ScanOptions, Scanner, Walker};
use std::path::PathBuf;
use std::time::Instant;

fn main() {
    let mut threads = None;
    let mut sink = "tree".to_string();
    let mut use_drive = false;
    let mut path = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--threads" => threads = args.next().and_then(|v| v.parse().ok()),
            "--sink" => sink = args.next().unwrap_or_default(),
            "--drive" => use_drive = true,
            other if !other.starts_with("--") => path = Some(PathBuf::from(other)),
            other => {
                eprintln!("unknown option {other}");
                std::process::exit(2);
            }
        }
    }
    let Some(path) = path else {
        eprintln!("usage: walk [--threads N] [--sink tree|drop] PATH");
        std::process::exit(2);
    };
    let mut options = ScanOptions {
        threads,
        ..ScanOptions::default()
    };
    if use_drive {
        match bs_platform::drives::volume_for_path(&path) {
            Ok(drive) => {
                options.os_used_bytes = drive.capacity.map(|c| c.used);
                // A mount that another mount of the drive already shows (a
                // bind mount) is left out, or its files would count twice.
                options.boundary = Boundary::with_mount_ids(
                    drive
                        .mounts
                        .iter()
                        .filter(|m| !m.covered_by_other_mount)
                        .filter_map(|m| m.mount_id)
                        .map(u64::from),
                );
                eprintln!(
                    "drive {} ({}), {} mounts, os_used={:?}",
                    drive.mount_point.display(),
                    drive.fs_type,
                    drive.mounts.len(),
                    options.os_used_bytes
                );
            }
            Err(e) => {
                eprintln!("cannot look up the drive: {e}");
                std::process::exit(1);
            }
        }
    }

    match sink.as_str() {
        "tree" => {
            let begun = Instant::now();
            let result = Walker::scan_blocking(&path, options);
            let elapsed = begun.elapsed();
            if let Some(e) = &result.stats.error {
                eprintln!("scan failed: {e}");
                std::process::exit(1);
            }
            let root = result.snapshot.root().expect("a root");
            println!(
                "sink=tree allocated={} logical={} files={} dirs={} nodes={} unreadable={} vanished={} threads={} batches={} parked={} elapsed_ms={:.3}",
                root.allocated(),
                root.logical(),
                root.file_count(),
                result.stats.progress.dirs,
                result.snapshot.len(),
                result.stats.progress.unreadable,
                result.stats.progress.vanished,
                result.stats.threads,
                result.stats.progress.batches,
                result.stats.records_parked,
                elapsed.as_secs_f64() * 1000.0,
            );
            // The application keeps its tree until it exits; so does this, and
            // the operating system reclaims the memory far faster than freeing
            // a million nodes one chunk at a time would.
            std::mem::forget(result);
        }
        "drop" => {
            let begun = Instant::now();
            let (ctx, rx) = ScanContext::channel(options, 64);
            let drain = std::thread::spawn(move || rx.iter().count());
            let end = Walker.scan(&path, &ctx);
            let progress = ctx.progress();
            drop(ctx);
            let batches = drain.join().unwrap_or(0);
            let elapsed = begun.elapsed();
            if let Err(e) = end {
                eprintln!("scan failed: {e}");
                std::process::exit(1);
            }
            println!(
                "sink=drop allocated={} logical={} files={} dirs={} unreadable={} vanished={} batches={} elapsed_ms={:.3}",
                progress.allocated_bytes,
                progress.logical_bytes,
                progress.files,
                progress.dirs,
                progress.unreadable,
                progress.vanished,
                batches,
                elapsed.as_secs_f64() * 1000.0,
            );
        }
        other => {
            eprintln!("unknown sink {other}");
            std::process::exit(2);
        }
    }
}
