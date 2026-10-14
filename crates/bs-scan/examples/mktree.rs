//! Builds a directory tree for the walker benchmark: many small files, a few
//! large ones, some hard links and symbolic links, nested a few levels deep.
//! The same arguments always make the same tree.
//!
//! ```text
//! mktree DIR [--files N] [--seed S]
//! ```
//!
//! Prints one line of `key=value` pairs describing what it made. If `DIR`
//! already holds a tree made with the same arguments it does nothing and says
//! `reused=1`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

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

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

const MAX_DEPTH: usize = 6;

fn name(rng: &mut Rng, prefix: &str) -> String {
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789_-";
    let len = 3 + rng.below(10) as usize;
    let mut s = String::from(prefix);
    for _ in 0..len {
        s.push(CHARS[rng.below(CHARS.len() as u64) as usize] as char);
    }
    s
}

/// A file size: mostly tiny, sometimes a few kilobytes, rarely large.
fn size(rng: &mut Rng) -> usize {
    match rng.below(100) {
        0..=69 => rng.below(1024) as usize,
        70..=94 => 1024 + rng.below(15 * 1024) as usize,
        _ => 16 * 1024 + rng.below(112 * 1024) as usize,
    }
}

#[cfg(unix)]
fn make_symlink(target: &str, at: &Path) -> bool {
    std::os::unix::fs::symlink(target, at).is_ok()
}

#[cfg(not(unix))]
fn make_symlink(_target: &str, _at: &Path) -> bool {
    false
}

fn main() {
    let mut dir: Option<PathBuf> = None;
    let mut files = 200_000u64;
    let mut seed = 1u64;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--files" => files = args.next().and_then(|v| v.parse().ok()).unwrap_or(files),
            "--seed" => seed = args.next().and_then(|v| v.parse().ok()).unwrap_or(seed),
            other if !other.starts_with("--") => dir = Some(PathBuf::from(other)),
            other => {
                eprintln!("unknown option {other}");
                std::process::exit(2);
            }
        }
    }
    let Some(dir) = dir else {
        eprintln!("usage: mktree DIR [--files N] [--seed S]");
        std::process::exit(2);
    };

    let marker = dir.join(".mktree");
    let wanted = format!("files={files} seed={seed}\n");
    if fs::read_to_string(&marker).is_ok_and(|m| m == wanted) {
        println!("reused=1 files={files} seed={seed}");
        return;
    }
    // Regenerating erases the directory, so only a directory this tool made
    // (it leaves the marker) or an empty one may be erased. A mistyped path
    // must not wipe a real folder.
    let made_here = marker.is_file();
    if !made_here && fs::read_dir(&dir).is_ok_and(|mut entries| entries.next().is_some()) {
        eprintln!(
            "refusing to use {}: it is not empty and was not made by mktree",
            dir.display()
        );
        std::process::exit(2);
    }
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create the dataset directory");

    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let zeros = vec![0u8; 128 * 1024];
    let mut made_files = 0u64;
    let mut made_dirs = 1u64;
    let mut links = 0u64;
    let mut symlinks = 0u64;
    let mut bytes = 0u64;
    let mut earlier: Vec<PathBuf> = Vec::new();
    // Breadth first, so the tree stays wide.
    let mut queue: std::collections::VecDeque<(PathBuf, usize)> = std::collections::VecDeque::new();
    queue.push_back((dir.clone(), 0));
    while let Some((here, depth)) = queue.pop_front() {
        if made_files >= files {
            break;
        }
        let per_dir = 10 + rng.below(50);
        for _ in 0..per_dir {
            if made_files >= files {
                break;
            }
            let roll = rng.below(100);
            let path = here.join(name(&mut rng, "f"));
            if roll < 3 && !earlier.is_empty() {
                let target = earlier[rng.below(earlier.len() as u64) as usize].clone();
                if fs::hard_link(&target, &path).is_ok() {
                    links += 1;
                    made_files += 1;
                    continue;
                }
            } else if roll < 5 {
                if make_symlink("../elsewhere", &path) {
                    symlinks += 1;
                    made_files += 1;
                }
                continue;
            }
            let n = size(&mut rng);
            let mut f = fs::File::create(&path).expect("create a file");
            let mut left = n;
            while left > 0 {
                let chunk = left.min(zeros.len());
                f.write_all(&zeros[..chunk]).expect("write a file");
                left -= chunk;
            }
            bytes += n as u64;
            made_files += 1;
            if earlier.len() < 50_000 && rng.below(10) == 0 {
                earlier.push(path);
            }
        }
        if depth < MAX_DEPTH {
            let subdirs = 2 + rng.below(6);
            for _ in 0..subdirs {
                let sub = here.join(name(&mut rng, "d"));
                if fs::create_dir(&sub).is_ok() {
                    made_dirs += 1;
                    queue.push_back((sub, depth + 1));
                }
            }
        }
    }
    // A few empty directories.
    for i in 0..20 {
        let _ = fs::create_dir(Path::new(&dir).join(format!("empty-{i}")));
        made_dirs += 1;
    }
    fs::write(&marker, wanted).expect("write the marker");
    println!(
        "reused=0 files={made_files} dirs={made_dirs} hard_links={links} symlinks={symlinks} bytes={bytes} seed={seed}"
    );
}
