//! Matches folders to the package or program that owns them, the way the
//! program does after a scan, and prints how long it took. Read-only.
//!
//! ```text
//! cargo run --release -p bs-platform --example software -- /usr/lib/firefox /opt/google
//! ```

use bs_platform::software::{Candidate, MIN_MATCH_BYTES, Matcher};
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn main() {
    let folders: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    if folders.is_empty() {
        eprintln!("usage: software <folder>...");
        return;
    }
    let candidates: Vec<Candidate> = folders
        .iter()
        .map(|path| Candidate {
            path: path.clone(),
            bytes: MIN_MATCH_BYTES,
        })
        .collect();
    let matcher = Matcher::new();
    let started = Instant::now();
    matcher.request(&candidates);
    let returned = started.elapsed();
    while !matcher.is_idle() {
        std::thread::sleep(Duration::from_millis(2));
    }
    let finished = started.elapsed();
    println!("request returned after {returned:?}; all answered after {finished:?}");
    for folder in &folders {
        println!("{}: {:?}", folder.display(), matcher.lookup(folder));
    }
}
