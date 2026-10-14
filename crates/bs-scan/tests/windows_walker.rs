//! The Windows walker against fixture trees. These run on the Windows CI legs;
//! on other systems the file is empty.
#![cfg(windows)]

use bs_scan::{ScanOptions, Walker};
use bs_tree::{NodeFlags, NodeKind, Snapshot};
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

struct Fixture(PathBuf);

impl Fixture {
    fn new(name: &str) -> Fixture {
        static N: AtomicU32 = AtomicU32::new(0);
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "walker-{}-{}-{name}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Fixture(root)
    }

    fn file(&self, rel: &str, len: usize) -> PathBuf {
        let p = self.0.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, vec![b'x'; len]).unwrap();
        p
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn scan(root: &Path) -> bs_scan::ScanResult {
    let result = Walker::scan_blocking(root, ScanOptions::default());
    assert!(result.stats.error.is_none(), "{:?}", result.stats.error);
    assert_eq!(result.stats.records_parked, 0);
    result.snapshot.verify().unwrap();
    assert!(result.snapshot.scan_complete());
    result
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
fn an_ordinary_tree_is_counted_from_the_directory_listing() {
    let fx = Fixture::new("ordinary");
    fx.file("a.txt", 10);
    fx.file("b/c.bin", 5000);
    fx.file("b/d/e.bin", 123_456);
    std::fs::create_dir_all(fx.0.join("b/empty")).unwrap();
    let result = scan(&fx.0);
    let snap = &result.snapshot;
    assert_eq!(snap.root().unwrap().logical(), 10 + 5000 + 123_456);
    assert_eq!(snap.root().unwrap().file_count(), 3);
    // Allocation comes from the listing: at least the data, in whole clusters
    // (resident data in the file table counts as zero).
    assert!(find(snap, "b/d/e.bin").allocated() >= 123_456);
    assert!(find(snap, "b/empty").is_complete());
}

#[test]
fn hard_links_are_counted_once_and_flagged() {
    let fx = Fixture::new("hardlinks");
    let original = fx.file("d1/original", 100_000);
    std::fs::create_dir_all(fx.0.join("d2")).unwrap();
    std::fs::hard_link(&original, fx.0.join("d2/second")).unwrap();
    let result = scan(&fx.0);
    let snap = &result.snapshot;
    let a = find(snap, "d1/original");
    let b = find(snap, "d2/second");
    assert!(a.is_hard_link() && b.is_hard_link());
    assert_eq!(a.logical() + b.logical(), 100_000);
    assert_eq!(snap.root().unwrap().logical(), 100_000);
}

#[test]
fn links_and_junctions_are_never_followed() {
    let fx = Fixture::new("links");
    let outside = Fixture::new("links-outside");
    outside.file("big/inner.bin", 5_000_000);
    fx.file("real/f.txt", 10);
    // A junction needs no privilege; a symbolic link may, and is skipped when
    // it cannot be made.
    let status = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(fx.0.join("junction"))
        .arg(&outside.0)
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stdout)
    );
    let made_symlink = std::os::windows::fs::symlink_dir(&outside.0, fx.0.join("symlink")).is_ok();
    let result = scan(&fx.0);
    let snap = &result.snapshot;
    assert!(
        snap.root().unwrap().logical() < 1_000_000,
        "nothing was followed"
    );
    let junction = find(snap, "junction");
    assert!(junction.flags().contains(NodeFlags::NOT_FOLLOWED));
    assert_eq!(junction.children().count(), 0);
    if made_symlink {
        let link = find(snap, "symlink");
        assert_eq!(link.kind(), NodeKind::Symlink);
        assert!(link.flags().contains(NodeFlags::NOT_FOLLOWED));
    }
}

#[test]
fn names_that_are_not_valid_utf_16_survive() {
    let fx = Fixture::new("names");
    // An unpaired surrogate is a legal Windows file name.
    let odd = OsString::from_wide(&[b'a' as u16, 0xD800, b'b' as u16]);
    std::fs::write(fx.0.join(&odd), b"x").unwrap();
    fx.file("caf\u{e9}/\u{65e5}\u{672c}.txt", 3);
    let result = scan(&fx.0);
    let snap = &result.snapshot;
    let names: Vec<Vec<u8>> = snap
        .root()
        .unwrap()
        .children()
        .map(|c| c.name().to_vec())
        .collect();
    assert!(
        names.contains(&vec![b'a', 0xED, 0xA0, 0x80, b'b']),
        "{names:?}"
    );
    assert_eq!(find(snap, "caf\u{e9}/\u{65e5}\u{672c}.txt").logical(), 3);
}

#[test]
fn a_path_longer_than_max_path_is_walked() {
    let fx = Fixture::new("long");
    // Build under the verbatim prefix, which lifts the 260 character limit.
    let mut dir = PathBuf::from(format!(r"\\?\{}", fx.0.display()));
    for _ in 0..40 {
        dir.push("a-directory-with-a-fairly-long-name");
    }
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("deep.txt"), b"deep").unwrap();
    let result = scan(&fx.0);
    assert_eq!(result.snapshot.root().unwrap().file_count(), 1);
    assert_eq!(result.snapshot.root().unwrap().logical(), 4);
}

#[test]
fn cancelling_keeps_a_valid_incomplete_tree() {
    let fx = Fixture::new("cancel");
    for d in 0..50 {
        fx.file(&format!("d{d}/f"), 10);
    }
    let handle = bs_scan::start(
        std::sync::Arc::new(Walker),
        fx.0.clone(),
        ScanOptions::default(),
    );
    handle.cancel();
    let result = handle.wait();
    result.snapshot.verify().unwrap();
    assert_eq!(result.stats.records_parked, 0);
}

/// Marks a file or directory `FILE_ATTRIBUTE_OFFLINE`, which is how a storage
/// provider marks data that is not on this machine. The attribute can be set
/// on any NTFS entry, so it stands in for a placeholder without a provider.
fn mark_offline(path: &Path) {
    let out = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command"])
        .arg(format!(
            "$ErrorActionPreference = 'Stop'; (Get-Item -LiteralPath '{}' -Force).Attributes = 'Offline'",
            path.display()
        ))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn offline_entries_are_counted_from_the_listing_and_never_listed() {
    let fx = Fixture::new("offline");
    let cloud_file = fx.file("cloud.bin", 5000);
    fx.file("cloud-dir/inside.bin", 7000);
    fx.file("plain/f.txt", 10);
    mark_offline(&cloud_file);
    mark_offline(&fx.0.join("cloud-dir"));
    let result = scan(&fx.0);
    let snap = &result.snapshot;

    // The file is counted for what the listing says, and flagged.
    let file = find(snap, "cloud.bin");
    assert!(file.flags().contains(NodeFlags::CLOUD_PLACEHOLDER));
    assert_eq!(file.logical(), 5000);
    // The directory is recorded but its contents are not read: listing a
    // placeholder folder can download what is in it.
    let dir = find(snap, "cloud-dir");
    assert!(dir.flags().contains(NodeFlags::CLOUD_PLACEHOLDER));
    assert!(dir.is_unreadable());
    assert_eq!(dir.children().count(), 0);
    assert!(snap.root().unwrap().bytes_are_lower_bound());
    // Ordinary neighbours are unaffected.
    assert_eq!(find(snap, "plain/f.txt").logical(), 10);
}

/// The root of a volume is where drive scans start. Its path ends in a
/// separator already, which must not be doubled when children are named, and
/// everything below it is reached through handles. A whole system drive is too
/// big to walk in a test, so the scan is stopped once two levels have been
/// listed.
#[test]
fn a_volume_root_is_walked_and_its_children_are_listed() {
    let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    let root = PathBuf::from(format!("{drive}\\"));
    let handle = bs_scan::start(
        std::sync::Arc::new(Walker),
        root,
        ScanOptions {
            threads: Some(4),
            ..ScanOptions::default()
        },
    );
    let reader = handle.reader();
    let started = std::time::Instant::now();
    loop {
        let snap = reader.load();
        let two_levels = snap
            .root()
            .is_some_and(|r| r.children().any(|c| c.children().next().is_some()));
        if two_levels || handle.is_done() || started.elapsed().as_secs() > 120 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    handle.cancel();
    let result = handle.wait();
    assert!(result.stats.error.is_none(), "{:?}", result.stats.error);
    result.snapshot.verify().unwrap();
    let top = result.snapshot.root().unwrap();
    assert!(top.name().ends_with(b"\\"), "the root keeps its separator");
    assert!(top.children().count() > 0, "the root was listed");
    assert!(
        top.children().any(|c| c.children().next().is_some()),
        "a directory below the root was opened and listed"
    );
    for child in top.children() {
        assert!(!child.name().contains(&b'\\'));
    }
}
