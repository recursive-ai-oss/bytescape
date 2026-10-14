//! A directory replaced by a junction between being listed and being opened
//! must not be followed. The file system is changed at exactly that moment,
//! through a hook, so the test does not depend on winning a race. One test per
//! file because the hook is process-wide.
//!
//! Runs on the Windows CI legs; on other systems the file is empty.
#![cfg(windows)]

use bs_scan::{ScanOptions, Walker};
use bs_tree::{NodeFlags, NodeKind, ReadErrorKind, Snapshot};
use std::path::{Path, PathBuf};
use std::process::Command;

struct Fixture(PathBuf);

impl Fixture {
    fn new(name: &str) -> Fixture {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("winswap-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Fixture(root)
    }

    fn file(&self, rel: &str, len: usize) {
        let p = self.0.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, vec![b'x'; len]).unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn find<'a>(snap: &'a Snapshot, rel: &str) -> bs_tree::NodeRef<'a> {
    let mut at = snap.root().unwrap();
    for part in rel.split('/') {
        at = at
            .children()
            .find(|c| c.name() == part.as_bytes())
            .unwrap_or_else(|| panic!("no {part} in {rel}"));
    }
    at
}

#[test]
fn a_directory_swapped_for_a_junction_before_it_is_opened_is_refused() {
    let fx = Fixture::new("exact");
    let outside = Fixture::new("exact-outside");
    // Following the junction would add five megabytes.
    outside.file("trap/big.bin", 5_000_000);
    fx.file("victim/inner.txt", 100);
    fx.file("bystander/ok.txt", 200);

    let root = fx.0.clone();
    let target = outside.0.clone();
    bs_scan::set_before_open_hook(Some(Box::new(move |name| {
        if name == b"victim" {
            std::fs::remove_dir_all(root.join("victim")).unwrap();
            let made = Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(root.join("victim"))
                .arg(&target)
                .output()
                .unwrap();
            assert!(
                made.status.success(),
                "{}",
                String::from_utf8_lossy(&made.stdout)
            );
        }
    })));
    let result = Walker::scan_blocking(&fx.0, ScanOptions::default());
    bs_scan::set_before_open_hook(None);

    assert!(result.stats.error.is_none(), "{:?}", result.stats.error);
    result.snapshot.verify().unwrap();
    let snap = &result.snapshot;
    assert!(snap.scan_complete());
    assert!(
        snap.root().unwrap().logical() < 1_000_000,
        "the junction was followed"
    );
    // It was listed as a directory, could not be opened as one, and says so.
    let victim = find(snap, "victim");
    assert_eq!(victim.kind(), NodeKind::Dir);
    assert!(victim.is_unreadable());
    assert_eq!(victim.read_failure().unwrap().kind, ReadErrorKind::NotFound);
    assert_eq!(victim.children().count(), 0);
    assert!(snap.root().unwrap().bytes_are_lower_bound());
    // The rest of the tree is intact.
    assert_eq!(find(snap, "bystander/ok.txt").logical(), 200);
    assert!(
        !find(snap, "bystander")
            .flags()
            .contains(NodeFlags::NOT_FOLLOWED)
    );
}
