//! The Windows side of the scan root's judgement: the helper opens the root
//! once, judges that handle, and holds the scan to it. These run on the
//! Windows CI legs; the Linux counterparts are in `pin_namespace_tests`.
//!
//! Every test works in a folder made for it under the temporary folder and
//! removes only that.

// The tests make links and mount points with the system's own tools; the
// helper itself starts nothing.
#![allow(clippy::disallowed_types, clippy::disallowed_methods)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use super::{Host, prepare};
use crate::plan::Chosen;
use crate::volume::{NotJudged, judge_scan_root};
use bs_scan::{RootPin, ScanContext, ScanError, Scanner};
use std::fs::OpenOptions;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::OwnedHandle;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

const FILE_READ_ATTRIBUTES: u32 = 0x80;
const FILE_SHARE_ALL: u32 = 7;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

/// A folder made for one test, in its real (resolved) form as the scan
/// request carries it. Links and mount points inside are taken away first, so
/// removing the folder never reaches what they point to.
struct Fixture {
    root: PathBuf,
    links: std::cell::RefCell<Vec<PathBuf>>,
    mounts: std::cell::RefCell<Vec<PathBuf>>,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let base = std::env::temp_dir().join(format!("bs-judge-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        Fixture {
            root: std::fs::canonicalize(&base).unwrap(),
            links: std::cell::RefCell::new(Vec::new()),
            mounts: std::cell::RefCell::new(Vec::new()),
        }
    }

    fn p(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    fn file(&self, relative: &str) {
        let path = self.p(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "1").unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for mount in self.mounts.borrow().iter() {
            let _ = Command::new("mountvol").arg(mount).arg("/D").output();
        }
        for link in self.links.borrow().iter() {
            let _ = std::fs::remove_dir(link);
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The pin a caller would make after judging `path` by another route: the
/// folder opened for attributes only, sharing everything.
fn pin_of(path: &Path) -> RootPin {
    let file = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_ALL)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .unwrap();
    RootPin::from_handle(Arc::new(OwnedHandle::from(file))).unwrap()
}

enum Counted {
    Walked(u64),
    Moved { sent: u64 },
}

fn scan_prepared(plan: crate::plan::Plan, options: bs_scan::ScanOptions, root: &Path) -> Counted {
    let (ctx, batches) = ScanContext::channel(options, 1 << 16);
    let result = Chosen::new(plan).scan(root, &ctx);
    let mut sent = 0u64;
    let mut aggregator = bs_tree::Aggregator::new();
    while let Ok(batch) = batches.try_recv() {
        sent += 1;
        aggregator.apply(&batch);
    }
    aggregator.finish();
    match result {
        Ok(_) => Counted::Walked(
            aggregator
                .publish()
                .root()
                .map_or(0, |r| u64::from(r.file_count())),
        ),
        Err(ScanError::RootMoved { .. }) => Counted::Moved { sent },
        Err(other) => panic!("the scan failed in another way: {other}"),
    }
}

fn not_run(why: &str) {
    eprintln!("NOT RUN: {why}");
}

#[test]
fn a_folder_is_judged_and_the_pin_is_that_folder() {
    let fx = Fixture::new("local");
    fx.file("a/one");
    let judged = judge_scan_root(&fx.p("a"), &Host::system())
        .unwrap_or_else(|e| panic!("a folder of this computer was not judged fit: {e:?}"));
    // The same key as bs-scan's own for that folder, so a walker that opens
    // the folder passes and any other folder does not.
    assert_eq!(judged.pin.key(), pin_of(&fx.p("a")).key());
    fx.file("b/one");
    assert_ne!(judged.pin.key(), pin_of(&fx.p("b")).key());
    assert_eq!(judged.mount_id, None);
    assert!(judged.os_used.is_some(), "no figure for the volume's use");
}

#[test]
fn a_judged_folder_scans_with_its_pin_in_the_options() {
    let fx = Fixture::new("scan");
    fx.file("a/one");
    fx.file("a/sub/two");
    let (plan, options) = prepare(&fx.p("a"), bs_ipc::Backend::Auto, &[], &[], &Host::system())
        .map_err(|e| format!("{e:?}"))
        .expect("judged");
    assert!(options.pinned_root.is_some());
    assert!(options.os_used_bytes.is_some());
    assert!(matches!(
        scan_prepared(plan, options, &fx.p("a")),
        Counted::Walked(2)
    ));
}

#[test]
fn a_file_as_the_root_is_refused() {
    let fx = Fixture::new("file");
    fx.file("plain");
    assert!(matches!(
        judge_scan_root(&fx.p("plain"), &Host::system()),
        Err(NotJudged::Unknown)
    ));
    assert!(matches!(
        judge_scan_root(&fx.p("missing"), &Host::system()),
        Err(NotJudged::Unknown)
    ));
}

#[test]
fn a_junction_as_the_root_is_refused() {
    let fx = Fixture::new("junction");
    fx.file("target/one");
    let made = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(fx.p("link"))
        .arg(fx.p("target"))
        .output()
        .unwrap();
    if !made.status.success() {
        return not_run(&format!(
            "cannot make a junction here: {}",
            String::from_utf8_lossy(&made.stdout)
        ));
    }
    fx.links.borrow_mut().push(fx.p("link"));
    assert!(matches!(
        judge_scan_root(&fx.p("link"), &Host::system()),
        Err(NotJudged::Unknown)
    ));
    // Beside it, the folder it leads to is judged as itself.
    assert!(judge_scan_root(&fx.p("target"), &Host::system()).is_ok());
}

#[test]
fn a_directory_symlink_as_the_root_is_refused() {
    let fx = Fixture::new("symlink");
    fx.file("target/one");
    if let Err(error) = std::os::windows::fs::symlink_dir(fx.p("target"), fx.p("link")) {
        return not_run(&format!("cannot make a symbolic link here: {error}"));
    }
    fx.links.borrow_mut().push(fx.p("link"));
    assert!(matches!(
        judge_scan_root(&fx.p("link"), &Host::system()),
        Err(NotJudged::Unknown)
    ));
}

#[test]
fn a_share_name_for_a_local_folder_is_a_network_volume() {
    let fx = Fixture::new("share");
    fx.file("a/one");
    // `C:\x` as the administrative share of this computer.
    let text = fx.p("a").to_string_lossy().into_owned();
    let plain = text.strip_prefix(r"\\?\").unwrap_or(&text);
    let (drive, rest) = plain.split_at(2);
    let share = PathBuf::from(format!(r"\\localhost\{}${rest}", &drive[..1]));
    if std::fs::metadata(&share).is_err() {
        return not_run("this computer has no administrative share for the drive");
    }
    assert!(
        matches!(
            judge_scan_root(&share, &Host::system()),
            Err(NotJudged::Remote)
        ),
        "a folder named through a share was not taken for a network volume"
    );
}

#[test]
fn a_folder_swapped_after_the_check_is_refused_with_nothing_sent() {
    let fx = Fixture::new("swap");
    fx.file("a/one");
    let host = Host::system();
    // Control: the judged folder, unmoved, scans.
    let (plan, options) = prepare(&fx.p("a"), bs_ipc::Backend::Auto, &[], &[], &host)
        .map_err(|e| format!("{e:?}"))
        .expect("judged");
    assert!(matches!(
        scan_prepared(plan, options, &fx.p("a")),
        Counted::Walked(1)
    ));
    let (plan, options) = prepare(&fx.p("a"), bs_ipc::Backend::Auto, &[], &[], &host)
        .map_err(|e| format!("{e:?}"))
        .expect("judged");
    // After the check another folder takes the name.
    std::fs::rename(fx.p("a"), fx.p("a-judged")).unwrap();
    fx.file("a/x");
    fx.file("a/y");
    fx.file("a/z");
    match scan_prepared(plan, options, &fx.p("a")) {
        Counted::Moved { sent } => assert_eq!(sent, 0, "something was sent"),
        Counted::Walked(n) => panic!("walked the folder that took the name: {n} files"),
    }
}

/// The drive of the temporary folder, as `X:\`, and the GUID name of its
/// volume, if the system gives one.
fn volume_of_temp() -> Option<(String, String)> {
    let temp = std::env::temp_dir();
    let text = temp.to_string_lossy().into_owned();
    let drive = format!("{}:\\", text.chars().next()?);
    let out = Command::new("mountvol")
        .arg(&drive)
        .arg("/L")
        .output()
        .ok()?;
    let guid = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && guid.starts_with(r"\\?\Volume{")).then_some((drive, guid))
}

/// A volume mounted on a folder is judged as the volume's own root once the
/// request's path is resolved, as the interface's scan request always is, and
/// a mount point that is not resolved is never taken for the folder it sits
/// in.
#[test]
fn a_volume_mounted_in_a_folder_is_judged_as_the_volume_and_never_as_the_folder_it_sits_in() {
    let Some((_, guid)) = volume_of_temp() else {
        return not_run("the system gives no volume name for the temporary folder's drive");
    };
    let fx = Fixture::new("mountpoint");
    std::fs::create_dir_all(fx.p("mnt")).unwrap();
    let made = Command::new("mountvol")
        .arg(fx.p("mnt"))
        .arg(&guid)
        .output()
        .unwrap();
    if !made.status.success() {
        return not_run(&format!(
            "cannot mount a volume on a folder here: {}",
            String::from_utf8_lossy(&made.stdout)
        ));
    }
    fx.mounts.borrow_mut().push(fx.p("mnt"));
    let host = Host::system();
    // As the scan request resolves it.
    let resolved = std::fs::canonicalize(fx.p("mnt")).unwrap();
    let judged = judge_scan_root(&resolved, &host);
    match &judged {
        Ok(judged) => assert_eq!(judged.pin.key(), pin_of(&resolved).key()),
        Err(NotJudged::CannotPin(error)) => {
            return not_run(&format!("the volume gives no file IDs: {error}"));
        }
        Err(other) => panic!("the volume's own root was not judged fit: {other:?}"),
    }
    // As written, the mount point itself: either refused as a link-like
    // folder, or judged as the volume's root, never the parent's folder.
    match judge_scan_root(&fx.p("mnt"), &host) {
        Err(NotJudged::Unknown) => eprintln!("a mount point as written is refused"),
        Ok(judged) => {
            eprintln!("a mount point as written is judged as the volume's root");
            assert_eq!(judged.pin.key(), pin_of(&resolved).key());
        }
        Err(other) => panic!("unexpected: {other:?}"),
    }
}
