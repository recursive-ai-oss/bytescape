//! Shared helpers for the walker tests: fixture directories under the build's
//! temporary directory (never `/tmp`), the reference `du`, and tree lookups.
#![allow(dead_code)]

use bs_scan::{ScanOptions, ScanResult, Walker};
use bs_tree::{NodeId, NodeRef, Snapshot};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

/// A fresh empty directory for one test. Removed when dropped (permissions
/// are restored first, so an unreadable fixture directory does not block
/// cleanup).
pub struct Fixture {
    pub root: PathBuf,
}

impl Fixture {
    pub fn new(name: &str) -> Fixture {
        static N: AtomicU32 = AtomicU32::new(0);
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "walker-{}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed),
            name
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Fixture { root }
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    pub fn dir(&self, rel: &str) -> PathBuf {
        let p = self.path(rel);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    pub fn file(&self, rel: &str, len: usize) -> PathBuf {
        let p = self.path(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, vec![b'x'; len]).unwrap();
        p
    }

    /// Makes the file system settle (delayed allocation would otherwise change
    /// block counts between two measurements).
    pub fn settle(&self) {
        let _ = Command::new("sync").status();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Restore access to anything a test made unreadable, then remove it.
        let _ = Command::new("chmod")
            .args(["-R", "u+rwx"])
            .arg(&self.root)
            .status();
        // `rm` copes with trees deeper than a path can name.
        let _ = Command::new("rm").arg("-rf").arg(&self.root).status();
    }
}

/// Runs a scan to the end with default options.
pub fn scan(root: &Path) -> ScanResult {
    scan_with(root, ScanOptions::default())
}

pub fn scan_with(root: &Path, options: ScanOptions) -> ScanResult {
    Walker::scan_blocking(root, options)
}

/// Checks what every finished scan of a quiet tree must satisfy.
pub fn assert_clean(result: &ScanResult) {
    assert!(
        result.stats.error.is_none(),
        "scan failed: {:?}",
        result.stats.error
    );
    assert_eq!(result.stats.end, Some(bs_scan::ScanEnd::Completed));
    assert_eq!(
        result.stats.records_parked, 0,
        "the walker must send parents before children"
    );
    assert_eq!(result.stats.records_rejected, 0);
    result.snapshot.verify().expect("tree invariants");
    assert!(result.snapshot.scan_complete());
}

/// The GNU `du` on this machine (`du` may be another implementation).
fn gnu_du() -> Option<&'static str> {
    for cmd in ["du", "gnudu"] {
        if let Ok(out) = Command::new(cmd).arg("--version").output()
            && String::from_utf8_lossy(&out.stdout).contains("GNU coreutils")
        {
            return Some(cmd);
        }
    }
    None
}

/// `du -s -B1` (size on disk) or `du -s --apparent-size -B1` of a path.
/// `None` when no GNU `du` is installed.
pub fn du(path: &Path, apparent: bool) -> Option<u64> {
    let cmd = gnu_du()?;
    let mut c = Command::new(cmd);
    c.env("LC_ALL", "C").args(["-s", "-B1"]);
    if apparent {
        c.arg("--apparent-size");
    }
    let out = c.arg(path).output().ok()?;
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Asserts the tree's totals equal GNU `du` on the same quiet fixture.
pub fn assert_matches_du(path: &Path, result: &ScanResult) {
    let root = result.snapshot.root().unwrap();
    match (du(path, false), du(path, true)) {
        (Some(on_disk), Some(apparent)) => {
            assert_eq!(root.allocated(), on_disk, "size on disk vs du -sB1");
            assert_eq!(
                root.logical(),
                apparent,
                "logical size vs du --apparent-size"
            );
        }
        _ => panic!("GNU du is required for this test and was not found"),
    }
}

/// The node at a `/`-separated path below the root.
pub fn node_at<'a>(snap: &'a Snapshot, rel: &str) -> NodeRef<'a> {
    let mut at = snap.root().unwrap();
    for part in rel.split('/').filter(|p| !p.is_empty()) {
        at = at
            .children()
            .find(|c| c.name() == part.as_bytes())
            .unwrap_or_else(|| panic!("no {part:?} in {rel:?}"));
    }
    at
}

pub fn try_node_at<'a>(snap: &'a Snapshot, rel: &str) -> Option<NodeRef<'a>> {
    let mut at = snap.root()?;
    for part in rel.split('/').filter(|p| !p.is_empty()) {
        at = at.children().find(|c| c.name() == part.as_bytes())?;
    }
    Some(at)
}

pub fn all_nodes(snap: &Snapshot) -> impl Iterator<Item = NodeRef<'_>> {
    (0..snap.len()).filter_map(|i| snap.node(NodeId::from_index(i)))
}

/// Whether this process runs as root, for which permission tests mean nothing.
pub fn is_root() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))
                .and_then(|l| l.split_whitespace().nth(2).map(|u| u == "0"))
        })
        .unwrap_or(false)
}
