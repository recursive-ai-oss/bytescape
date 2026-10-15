//! Reads a mounted btrfs file system with the tree-search reader and prints a
//! line of totals, for timing and for comparing with the walker. Needs
//! administrator rights.
//!
//! scan [--no-categories] [--no-census] [--runs N] [--quiet] DIRECTORY
#[cfg(all(target_os = "linux", target_endian = "little"))]
mod linux {
    use bs_scan::{ScanOptions, start};
    use bs_scan_btrfs::{BtrfsOptions, BtrfsScanner};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Instant;

    pub fn main() {
        let mut dir: Option<PathBuf> = None;
        let mut options = BtrfsOptions::default();
        let mut categories = true;
        let mut runs = 1u32;
        let mut quiet = false;
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--no-categories" => categories = false,
                "--no-census" => options.census = false,
                "--quiet" => quiet = true,
                "--runs" => runs = args.next().and_then(|n| n.parse().ok()).unwrap_or(1),
                other => dir = Some(PathBuf::from(other)),
            }
        }
        let Some(dir) = dir else {
            eprintln!("usage: scan [--no-categories] [--no-census] [--runs N] [--quiet] DIRECTORY");
            std::process::exit(2);
        };
        for run in 0..runs {
            let scanner = Arc::new(BtrfsScanner::new(options.clone()));
            let begun = Instant::now();
            let handle = start(
                scanner.clone(),
                dir.clone(),
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
                "sink=tree allocated={} logical={} files={} nodes={} complete={} trees={} census_trees={} shared_files={} unreachable={} memory_cap={} memory_used={} load_ms={:.3} emit_ms={:.3} elapsed_ms={:.3} run={run}",
                root.allocated(),
                root.logical(),
                root.file_count(),
                result.snapshot.len(),
                result.snapshot.scan_complete(),
                report.trees,
                report.census_trees,
                report.sharing.files,
                report.unreachable.allocated,
                report.memory_cap_bytes,
                report.memory_used_bytes,
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

#[cfg(all(target_os = "linux", target_endian = "little"))]
fn main() {
    linux::main();
}

#[cfg(not(all(target_os = "linux", target_endian = "little")))]
fn main() {
    eprintln!("the btrfs reader is for Linux");
}
