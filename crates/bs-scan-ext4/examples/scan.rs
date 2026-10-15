//! Reads an ext4 image or device and prints what the reader found.
//!
//!     cargo run --release -p bs-scan-ext4 --example scan -- IMAGE [--no-categories]
//!         [--skip PATH]... [--threads N] [--runs N] [--quiet]
//!
//! `--skip` names a directory below the file system's root that is a mount
//! point of another file system. With `--runs N` the scan repeats N times and
//! prints one line per run, for timing.
#[cfg(target_os = "linux")]
mod linux {
    use bs_scan::{ScanOptions, start};
    use bs_scan_ext4::{Ext4Options, Ext4Scanner};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Instant;

    pub fn main() {
        let mut image: Option<PathBuf> = None;
        let mut options = Ext4Options::default();
        let mut categories = true;
        let mut runs = 1u32;
        let mut quiet = false;
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--no-categories" => categories = false,
                "--quiet" => quiet = true,
                "--skip" => options
                    .skip
                    .push(args.next().unwrap_or_default().into_bytes()),
                "--threads" => options.threads = args.next().and_then(|n| n.parse().ok()),
                "--runs" => runs = args.next().and_then(|n| n.parse().ok()).unwrap_or(1),
                other => image = Some(PathBuf::from(other)),
            }
        }
        let Some(image) = image else {
            eprintln!(
                "usage: scan IMAGE [--no-categories] [--skip PATH]... [--threads N] [--runs N] [--quiet]"
            );
            std::process::exit(2);
        };
        for run in 0..runs {
            let scanner = Arc::new(Ext4Scanner::new(&image, options.clone()));
            let begun = Instant::now();
            let handle = start(
                scanner.clone(),
                PathBuf::from("/"),
                ScanOptions {
                    categories,
                    ..ScanOptions::default()
                },
            );
            let result = handle.wait();
            let elapsed = begun.elapsed();
            let Some(root) = result.snapshot.root() else {
                eprintln!("no tree: {:?}", result.stats.error);
                std::process::exit(1);
            };
            let report = scanner.last_report().unwrap_or_default();
            if !quiet {
                println!("{report:#?}");
            }
            println!(
                "sink=tree allocated={} logical={} files={} nodes={} complete={} threads={} reserved={} unreachable={} hidden={} memory_cap={} needs_recovery={} load_ms={:.3} emit_ms={:.3} elapsed_ms={:.3} run={run}",
                root.allocated(),
                root.logical(),
                root.file_count(),
                result.snapshot.len(),
                result.snapshot.scan_complete(),
                result.stats.threads,
                report.reserved.allocated,
                report.unreachable.allocated,
                report.hidden_under_mounts.allocated,
                report.memory_cap_bytes,
                report.needs_recovery,
                report.load_time.as_secs_f64() * 1000.0,
                report.emit_time.as_secs_f64() * 1000.0,
                elapsed.as_secs_f64() * 1000.0,
            );
            if let Some(e) = &result.stats.error {
                eprintln!("scan error: {e}");
            }
            // The application keeps its tree until it exits; so does this.
            std::mem::forget(result);
        }
    }
}

#[cfg(target_os = "linux")]
fn main() {
    linux::main();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("the ext4 reader is for Linux");
}
