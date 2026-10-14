//! Times `classify`, `recognize` and `protection` over a mixed set of paths.
//!
//! ```text
//! cargo run --release -p bs-knowledge --example classify_bench [CALLS [classify|recognize|protection]]
//! ```
//!
//! With one function named, only that one runs. Run it under `valgrind` with
//! two different call counts: if the total number of allocations is the same,
//! the calls themselves allocate nothing.

use bs_knowledge::{Environment, Knowledge};
use std::hint::black_box;
use std::time::Instant;

fn corpus() -> Vec<(Vec<u8>, bool)> {
    let mut paths: Vec<(String, bool)> = Vec::new();
    let linux_dirs = [
        "/home/alice/Videos",
        "/home/alice/Documents/work/reports/2024",
        "/home/alice/.cache/mozilla/firefox/abcd.default/cache2/entries",
        "/home/alice/projects/app/node_modules/left-pad/lib",
        "/home/alice/.cargo/registry/src/index.crates.io/serde-1.0/src",
        "/var/lib/docker/overlay2/layer-one/diff/usr/lib",
        "/usr/share/doc/package",
        "/tmp",
        "/mnt/data/backups/2023/photos",
        "/home/alice/.local/share/Steam/steamapps/common/Game/data",
    ];
    let windows_dirs = [
        r"C:\Users\alice\Videos",
        r"C:\Users\alice\AppData\Local\Temp\scratch",
        r"C:\Users\alice\AppData\Local\Google\Chrome\User Data\Default\Cache\Cache_Data",
        r"C:\Windows\WinSxS\amd64_microsoft-windows-x_31bf3856ad364e35",
        r"C:\Program Files\Some Program\resources",
        r"D:\Projects\app\node_modules\pkg\dist",
        r"D:\Photos\2023\Trip",
    ];
    let files = [
        "movie.mkv",
        "photo.JPG",
        "song.flac",
        "report.pdf",
        "backup.tar.gz",
        "setup.exe",
        "main.rs",
        "index.js",
        "data.tmp",
        "README",
        "notes.txt",
        "library.so",
        "image.iso",
        "archive.zip",
        "clip.mp4",
        "sheet.xlsx",
    ];
    for dir in linux_dirs.iter().chain(windows_dirs.iter()) {
        let sep = if dir.contains('\\') { '\\' } else { '/' };
        for file in files {
            paths.push((format!("{dir}{sep}{file}"), false));
        }
        paths.push((dir.to_string(), true));
    }
    paths
        .into_iter()
        .map(|(p, d)| (p.into_bytes(), d))
        .collect()
}

fn main() {
    let mut args = std::env::args().skip(1);
    let calls: usize = args
        .next()
        .and_then(|a| a.parse().ok())
        .unwrap_or(1_000_000);
    let only = args.next();

    let env = Environment::new()
        .with("HOME", "/home/alice")
        .with("USERPROFILE", r"C:\Users\alice")
        .with("LOCALAPPDATA", r"C:\Users\alice\AppData\Local")
        .with("APPDATA", r"C:\Users\alice\AppData\Roaming")
        .with("ProgramData", r"C:\ProgramData")
        .with("ProgramFiles", r"C:\Program Files")
        .with("ProgramFiles(x86)", r"C:\Program Files (x86)")
        .with("SystemRoot", r"C:\Windows")
        .with("SystemDrive", "C:");

    let started = Instant::now();
    let knowledge = Knowledge::new(&env);
    println!("Knowledge::new: {:?}", started.elapsed());

    let corpus = corpus();
    println!("corpus: {} paths", corpus.len());

    for name in ["classify", "recognize", "protection"] {
        if only.as_deref().is_some_and(|o| o != name) {
            continue;
        }
        let started = Instant::now();
        let mut sink = 0usize;
        for (path, is_dir) in corpus.iter().cycle().take(calls) {
            let path = black_box(path.as_slice());
            sink += match name {
                "classify" => knowledge.classify_bytes(path, *is_dir).number() as usize,
                "recognize" => usize::from(knowledge.recognize_bytes(path).is_some()),
                _ => knowledge.protection_bytes(path).level as usize,
            };
        }
        let elapsed = started.elapsed();
        println!(
            "{name:<10} {calls} calls in {elapsed:?}: {:.0} ns/call (checksum {sink})",
            elapsed.as_nanos() as f64 / calls as f64,
        );
    }
}
