//! Reads an NTFS volume or image and prints what it found: the totals, the
//! report, and optionally every path. For timing and inspection.
//!
//! `scan <image-or-\\.\C:> [--listing] [--threads N] [--paths] [--sep /]`

use bs_scan::{ScanOptions, start};
use bs_scan_ntfs::{Accounting, NtfsOptions, NtfsScanner};
use bs_tree::NodeKind;
use std::path::PathBuf;
use std::sync::Arc;

fn main() {
    let mut device = None;
    let mut options = NtfsOptions::default();
    let mut paths = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--listing" => options.accounting = Accounting::Listing,
            "--paths" => paths = true,
            "--threads" => options.threads = args.next().and_then(|n| n.parse().ok()),
            "--sep" => {
                options.path_separator =
                    args.next().and_then(|s| s.bytes().next()).unwrap_or(b'\\');
            }
            other => device = Some(PathBuf::from(other)),
        }
    }
    let Some(device) = device else {
        eprintln!("usage: scan <image-or-volume> [--listing] [--threads N] [--paths] [--sep C]");
        std::process::exit(2);
    };
    let scanner = Arc::new(NtfsScanner::new(device, options));
    let begun = std::time::Instant::now();
    let handle = start(
        scanner.clone(),
        PathBuf::from("C:\\"),
        ScanOptions::default(),
    );
    let result = handle.wait();
    let elapsed = begun.elapsed();
    if let Some(e) = &result.stats.error {
        eprintln!("scan failed: {e}");
        std::process::exit(1);
    }
    let root = result.snapshot.root().expect("a root");
    println!(
        "elapsed {:.1} ms; nodes {}; allocated {}; logical {}; files {}; complete {}",
        elapsed.as_secs_f64() * 1e3,
        result.snapshot.len(),
        root.allocated(),
        root.logical(),
        root.file_count(),
        result.snapshot.scan_complete(),
    );
    if let Some(r) = scanner.last_report() {
        println!("{r:#?}");
    }
    if paths {
        let mut stack = vec![(root, Vec::<u8>::new())];
        while let Some((node, path)) = stack.pop() {
            for child in node.children() {
                let mut p = path.clone();
                p.push(b'/');
                p.extend_from_slice(child.name());
                println!(
                    "{}\t{:?}\t{}\t{}\t{}\t{:?}",
                    String::from_utf8_lossy(&p),
                    child.kind(),
                    child.logical(),
                    child.allocated(),
                    child.mtime(),
                    child.flags()
                );
                if child.kind() == NodeKind::Dir {
                    stack.push((child, p));
                }
            }
        }
    }
}
