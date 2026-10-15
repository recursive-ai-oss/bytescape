//! The Windows walker holds a pinned scan to the folder its caller judged: the
//! volume serial number and the 128-bit file ID of the root, compared on the
//! handle the walker opened itself, before anything is read or sent. These run
//! on the Windows CI legs; on other systems the file is empty.
#![cfg(windows)]

use bs_scan::{RootKey, RootPin, ScanError, ScanOptions, ScanResult, Walker};
use std::fs::OpenOptions;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::OwnedHandle;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

const FILE_READ_ATTRIBUTES: u32 = 0x80;
const FILE_SHARE_ALL: u32 = 7;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

struct Fixture(PathBuf);

impl Fixture {
    fn new(name: &str) -> Fixture {
        static N: AtomicU32 = AtomicU32::new(0);
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "winpin-{}-{}-{name}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Fixture(root)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The pin a caller would make after judging the folder `path`: it opens the
/// folder once, for attributes only and sharing everything, and gives the
/// handle to the pin.
fn pin_of(path: &Path) -> RootPin {
    let file = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_ALL)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .unwrap();
    RootPin::from_handle(Arc::new(OwnedHandle::from(file))).unwrap()
}

fn assert_nothing_sent(result: &ScanResult) {
    assert!(result.snapshot.root().is_none(), "a root was sent");
    let p = result.stats.progress;
    assert_eq!((p.batches, p.files, p.dirs), (0, 0, 0), "{p:?}");
    assert_eq!(result.stats.records_applied, 0);
}

fn fixture(name: &str) -> Fixture {
    let fx = Fixture::new(name);
    std::fs::create_dir_all(fx.0.join("a/b")).unwrap();
    std::fs::write(fx.0.join("a/one"), vec![b'x'; 1000]).unwrap();
    std::fs::write(fx.0.join("a/b/two"), vec![b'x'; 3000]).unwrap();
    fx
}

#[test]
fn a_pin_has_the_volume_and_a_file_id() {
    let fx = fixture("key");
    let a = pin_of(&fx.0.join("a"));
    let again = pin_of(&fx.0.join("a"));
    let b = pin_of(&fx.0.join("a/b"));
    assert!(matches!(a.key(), RootKey::Volume { .. }), "{:?}", a.key());
    assert_eq!(a.key(), again.key(), "the same folder, the same key");
    assert_ne!(a.key(), b.key(), "another folder, another file ID");
    let (RootKey::Volume { serial: sa, .. }, RootKey::Volume { serial: sb, .. }) =
        (a.key(), b.key())
    else {
        panic!("not volume keys");
    };
    assert_eq!(sa, sb, "one volume");
}

#[test]
fn a_pin_on_the_roots_own_folder_runs_unchanged() {
    let fx = fixture("right");
    let root = fx.0.join("a");
    let plain = Walker::scan_blocking(&root, ScanOptions::default());
    assert!(plain.stats.error.is_none(), "{:?}", plain.stats.error);
    let result = Walker::scan_blocking(&root, ScanOptions::default().pin_root(pin_of(&root)));
    assert!(result.stats.error.is_none(), "{:?}", result.stats.error);
    result.snapshot.verify().unwrap();
    let (a, b) = (
        plain.snapshot.root().unwrap(),
        result.snapshot.root().unwrap(),
    );
    assert_eq!(
        (a.logical(), a.allocated(), plain.snapshot.len()),
        (b.logical(), b.allocated(), result.snapshot.len())
    );
}

#[test]
fn a_pin_on_another_folder_is_refused_with_nothing_sent() {
    let fx = fixture("other");
    let pin = pin_of(&fx.0.join("a/b"));
    let pinned_key = pin.key();
    let found_key = pin_of(&fx.0.join("a")).key();
    let result = Walker::scan_blocking(&fx.0.join("a"), ScanOptions::default().pin_root(pin));
    match &result.stats.error {
        Some(ScanError::RootMoved {
            pinned,
            found: Some(found),
        }) => assert_eq!((*pinned, *found), (pinned_key, found_key)),
        other => panic!("expected RootMoved, got {other:?}"),
    }
    assert_nothing_sent(&result);
    result.snapshot.verify().unwrap();
    assert!(!result.snapshot.scan_complete());
}

#[test]
fn a_folder_replaced_after_it_was_judged_is_refused() {
    let fx = fixture("swapped");
    let root = fx.0.join("a");
    // Judged, then moved aside and another folder put at the same name: the
    // name now leads to a folder with another file ID.
    let pin = pin_of(&root);
    std::fs::rename(&root, fx.0.join("a-moved")).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("other"), vec![b'y'; 5000]).unwrap();
    let result = Walker::scan_blocking(&root, ScanOptions::default().pin_root(pin));
    assert!(
        matches!(result.stats.error, Some(ScanError::RootMoved { .. })),
        "{:?}",
        result.stats.error
    );
    assert_nothing_sent(&result);
}

#[test]
fn without_a_pin_the_scan_is_as_before() {
    let fx = fixture("none");
    let result = Walker::scan_blocking(&fx.0.join("a"), ScanOptions::default());
    assert!(result.stats.error.is_none(), "{:?}", result.stats.error);
    assert!(ScanOptions::default().pinned_root.is_none());
}
