//! Prints the Recycle Bin or Trash of this machine's drives, or times the
//! measurement on a large made-up one.
//!
//! ```text
//! cargo run -p bs-platform --example bins
//! XDG_DATA_HOME=<dir>/data cargo run --release -p bs-platform --example bins -- --fixture <dir> 100000
//! ```
//!
//! The first form only reads. The second builds a Trash of the given number
//! of items under a folder that must not exist yet, points the home Trash at
//! it through `XDG_DATA_HOME`, and measures it twice. It never touches any
//! other folder.

use bs_platform::bins::{self, BinInfo, BinQuery};
use bs_platform::drives::{self, Drive, DriveKind};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1u64 << 20) as f64)
}

fn print_info(info: &BinInfo) {
    println!(
        "  {}: {} items, {} on disk, {} of files, {}",
        info.name.as_str(),
        info.items,
        info.allocated_bytes
            .map_or_else(|| "unknown size on disk".to_owned(), mib),
        mib(info.logical_bytes),
        if info.complete {
            "complete"
        } else {
            "INCOMPLETE"
        },
    );
    for location in &info.locations {
        println!("    {:?}: {}", location.role, location.path.display());
    }
    println!(
        "    shared files: {}, policy: {:?}",
        info.shared_files, info.policy
    );
    for problem in info.problems.iter().take(10) {
        println!("    problem: {problem:?}");
    }
    if info.problems.len() > 10 || info.problems_omitted > 0 {
        println!(
            "    ... {} more problems",
            info.problems.len().saturating_sub(10) as u64 + info.problems_omitted
        );
    }
}

fn wait(mut query: BinQuery) -> BinInfo {
    loop {
        if let Some(info) = query.wait_timeout(Duration::from_millis(200)) {
            return info;
        }
        let progress = query.progress();
        eprint!(
            "\r  ... {} entries, {}   ",
            progress.entries,
            mib(progress.allocated_bytes)
        );
    }
}

fn list() -> ExitCode {
    let drives = match drives::list_drives() {
        Ok(drives) => drives,
        Err(error) => {
            eprintln!("cannot list the drives: {error}");
            return ExitCode::FAILURE;
        }
    };
    for drive in drives.iter().filter(|d| d.kind != DriveKind::Other) {
        println!("{} ({:?})", drive.mount_point.display(), drive.kind);
        let started = Instant::now();
        let info = wait(bins::start(drive));
        eprintln!();
        print_info(&info);
        println!("  took {:?}", started.elapsed());
    }
    ExitCode::SUCCESS
}

/// Builds `items` trashed files: a 100 byte file and its info file each.
fn build(dir: &Path, items: u64) -> std::io::Result<()> {
    let trash = dir.join("data/Trash");
    fs::create_dir_all(trash.join("files"))?;
    fs::create_dir_all(trash.join("info"))?;
    let body = [b'x'; 100];
    for i in 0..items {
        // Spread them into folders now and then, as real trash has.
        let name = format!("item-{i:07}");
        if i % 10 == 0 {
            let folder = trash.join("files").join(&name);
            fs::create_dir(&folder)?;
            fs::File::create(folder.join("inner.txt"))?.write_all(&body)?;
        } else {
            fs::File::create(trash.join("files").join(&name))?.write_all(&body)?;
        }
        let info =
            format!("[Trash Info]\nPath=/home/user/{name}\nDeletionDate=2026-01-02T03:04:05\n");
        fs::write(trash.join("info").join(format!("{name}.trashinfo")), info)?;
    }
    Ok(())
}

fn fixture(dir: &Path, items: u64) -> ExitCode {
    let expected = dir.join("data");
    if std::env::var_os("XDG_DATA_HOME").as_deref() != Some(expected.as_os_str()) {
        eprintln!(
            "set XDG_DATA_HOME={} so the measurement reads the fixture and not your own Trash",
            expected.display()
        );
        return ExitCode::FAILURE;
    }
    if dir.exists() {
        eprintln!(
            "{} exists already; give a folder that does not",
            dir.display()
        );
        return ExitCode::FAILURE;
    }
    let started = Instant::now();
    if let Err(error) = build(dir, items) {
        eprintln!("cannot build the fixture: {error}");
        return ExitCode::FAILURE;
    }
    println!("built {items} items in {:?}", started.elapsed());

    let drive = Drive {
        id: drives::DriveId::new("fixture"),
        mount_point: dir.to_path_buf(),
        label: None,
        fs_type: String::new(),
        kind: DriveKind::Fixed,
        read_only: false,
        eligible_for_background_scan: false,
        capacity: None,
        capacity_problem: None,
        mounts: Vec::new(),
    };
    for run in ["first", "second"] {
        let started = Instant::now();
        let query = bins::start(&drive);
        // `start` returns at once; this is how long the caller is held up.
        println!("{run} run: start() returned after {:?}", started.elapsed());
        let info = wait(query);
        eprintln!();
        println!("{run} run: finished after {:?}", started.elapsed());
        print_info(&info);
    }
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => list(),
        [flag, dir, items] if flag == "--fixture" => match items.parse::<u64>() {
            Ok(items) => fixture(&PathBuf::from(dir), items),
            Err(_) => {
                eprintln!("the number of items must be a number");
                ExitCode::FAILURE
            }
        },
        _ => {
            eprintln!("usage: bins [--fixture <new folder> <items>]");
            ExitCode::FAILURE
        }
    }
}
