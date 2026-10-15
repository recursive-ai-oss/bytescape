//! The attack the pin exists for: a process that shares the user's account
//! changes what the scan root's path leads to after the caller judged the
//! folder and before the walker opened it, so that the walker lands on another
//! mount. A pinned scan is refused with nothing sent; an unpinned one (the
//! control, today's behaviour for the unprivileged application) walks the other
//! mount.
//!
//! The second attack is a remount: the judged mount is unmounted (lazily, so
//! it can go while something still has it open) and another file system is
//! mounted at the same place. The kernel hands a freed mount ID out again, so
//! a pin that remembered only the number would take the new mount for the
//! judged one. The pin holds the judged descriptor, which keeps that mount, and
//! so its ID, from being freed; and compares the never-reused ID where the
//! kernel has one. Both are tested: the second with the unique ID switched off,
//! as on a kernel before 6.8.
//!
//! The swap is made by a hook between resolving and opening the root, inside a
//! private user and mount namespace (`unshare --user --map-root-user
//! --mount`), so it needs no privilege on the host. Where the namespace cannot
//! be made the test prints `NOT RUN` and returns, unless
//! `BYTESCAPE_REQUIRE_NAMESPACES` is set, which turns that into a failure. One
//! inner test per file because the hook is process-wide.
#![cfg(target_os = "linux")]

mod support;

use bs_scan::{RootKey, RootPin, ScanError, ScanOptions, rawdir};
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use support::*;

fn run(cmd: &str, args: &[&str], dir: &Path) {
    let status = Command::new(cmd).args(args).arg(dir).status().unwrap();
    assert!(status.success(), "{cmd} {args:?} {dir:?} failed");
}

/// The pin a caller would make after judging the folder `path`. The caller
/// does not keep the descriptor: only the pin holds it.
fn pin_of(path: &Path) -> RootPin {
    RootPin::from_fd(Arc::new(rawdir::open_root(path).unwrap())).unwrap()
}

fn key_of(path: &Path) -> RootKey {
    pin_of(path).key()
}

/// The older mount ID the walker sees, which the kernel reuses.
fn legacy_id_of(path: &Path) -> u64 {
    let fd = rawdir::open_root(path).unwrap();
    rawdir::stat_fd(fd.as_fd()).unwrap().mnt_id.unwrap()
}

/// Moves the folder above `root` aside and puts a tmpfs where `root` was: what
/// the path leads to now is another mount.
fn swap_in_a_tmpfs(parent: PathBuf, root: PathBuf) {
    let aside = parent.with_extension("orig");
    let once = std::sync::atomic::AtomicBool::new(false);
    bs_scan::set_before_root_open_hook(Some(Box::new(move |resolved| {
        if resolved == root && !once.swap(true, std::sync::atomic::Ordering::SeqCst) {
            std::fs::rename(&parent, &aside).unwrap();
            std::fs::create_dir_all(&root).unwrap();
            run("mount", &["-t", "tmpfs", "-o", "size=8m", "none"], &root);
            std::fs::write(root.join("other-drive.bin"), vec![b'y'; 1 << 20]).unwrap();
        }
    })));
}

/// Undoes the swap so the next round starts from the same place.
fn put_back(parent: &Path, root: &Path) {
    bs_scan::set_before_root_open_hook(None);
    let aside = parent.with_extension("orig");
    if aside.exists() {
        run("umount", &[], root);
        std::fs::remove_dir_all(parent).unwrap();
        std::fs::rename(&aside, parent).unwrap();
    }
}

/// Runs inside the namespace; outside it (a plain `cargo test`) it has
/// nothing to do.
#[test]
fn inside_the_namespace_a_pinned_scan_is_not_led_to_another_mount() {
    let Some(dir) = std::env::var_os("BS_PIN_DIR") else {
        return;
    };
    let base = PathBuf::from(dir);
    let parent = base.join("a");
    let root = parent.join("root");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("mine.txt"), vec![b'x'; 300]).unwrap();
    let judged = pin_of(&root);

    // Control: no swap, pinned. It runs.
    let result = scan_with(&root, ScanOptions::default().pin_root(judged.clone()));
    assert!(result.stats.error.is_none(), "{:?}", result.stats.error);
    assert!(node_at(&result.snapshot, "mine.txt").logical() == 300);

    // Control: swapped, not pinned. The walker lands on the other mount and
    // counts it, which is the gap.
    swap_in_a_tmpfs(parent.clone(), root.clone());
    let result = scan(&root);
    let tmpfs = key_of(&root);
    put_back(&parent, &root);
    assert!(result.stats.error.is_none(), "{:?}", result.stats.error);
    assert_ne!(
        tmpfs,
        judged.key(),
        "the premise: the swap changed the mount"
    );
    assert!(
        node_at(&result.snapshot, "other-drive.bin").logical() >= (1 << 20),
        "the control did not reach the other mount"
    );

    // Swapped, pinned: refused, and nothing was read or sent.
    swap_in_a_tmpfs(parent.clone(), root.clone());
    let result = scan_with(&root, ScanOptions::default().pin_root(judged.clone()));
    let found = key_of(&root);
    put_back(&parent, &root);
    match &result.stats.error {
        Some(ScanError::RootMoved {
            pinned,
            found: Some(f),
        }) => assert_eq!((*pinned, *f), (judged.key(), found)),
        other => panic!("expected RootMoved, got {other:?}"),
    }
    assert!(result.snapshot.root().is_none(), "a root was sent");
    let p = result.stats.progress;
    assert_eq!((p.batches, p.files, p.dirs), (0, 0, 0), "{p:?}");

    // And the folder is back, so a pinned scan runs again.
    let result = scan_with(&root, ScanOptions::default().pin_root(judged.clone()));
    assert!(result.stats.error.is_none(), "{:?}", result.stats.error);

    // The remount, with the kernel's never-reused mount ID.
    remount_is_refused(&base.join("unique"), false);
    // The remount, as on a kernel that only has the reusable ID.
    remount_is_refused(&base.join("legacy"), true);
}

/// The judged mount is lazily unmounted and another file system mounted at the
/// same place; a pinned scan must be refused with nothing sent. With `legacy`
/// only the reusable mount ID is compared.
fn remount_is_refused(parent: &Path, legacy: bool) {
    let root = parent.join("root");
    std::fs::create_dir_all(&root).unwrap();
    bs_scan::force_legacy_mount_ids(legacy);
    run("mount", &["-t", "tmpfs", "-o", "size=4m", "judged"], &root);
    std::fs::write(root.join("judged.txt"), b"a").unwrap();
    // What the pin remembers, and what keeps the mount alive.
    let pin = pin_of(&root);
    assert!(
        matches!(pin.key(), RootKey::Mount { unique, .. } if unique == !legacy),
        "{:?}",
        pin.key()
    );
    let judged_id = legacy_id_of(&root);
    run("umount", &["-l"], &root);
    run("mount", &["-t", "tmpfs", "-o", "size=4m", "other"], &root);
    std::fs::write(root.join("other-mount.bin"), vec![b'y'; 65536]).unwrap();
    let result = scan_with(&root, ScanOptions::default().pin_root(pin.clone()));
    let found = key_of(&root);
    bs_scan::force_legacy_mount_ids(false);
    match &result.stats.error {
        Some(ScanError::RootMoved {
            pinned,
            found: Some(f),
        }) => {
            assert_eq!(*pinned, pin.key());
            assert_ne!(*f, *pinned);
            assert!(matches!(f, RootKey::Mount { .. }), "{found:?}");
        }
        other => panic!("expected RootMoved after a remount (legacy: {legacy}), got {other:?}"),
    }
    assert!(result.snapshot.root().is_none(), "a root was sent");
    let p = result.stats.progress;
    assert_eq!((p.batches, p.files, p.dirs), (0, 0, 0), "{p:?}");
    // Why the pin holds the descriptor: with the judged mount kept alive its
    // ID cannot be handed out again. (With the unique ID this does not matter
    // to the check, so only the reusable-ID case asserts it.)
    if legacy {
        assert_ne!(
            legacy_id_of(&root),
            judged_id,
            "the judged mount's ID was reused"
        );
    }
    run("umount", &[], &root);
}

#[test]
fn a_pinned_scan_is_not_led_to_another_mount_in_a_private_namespace() {
    if std::env::var_os("BS_PIN_DIR").is_some() {
        // This is the inner run; the other test does the work.
        return;
    }
    let probe = Command::new("unshare")
        .args(["--user", "--map-root-user", "--mount", "true"])
        .output();
    if !matches!(&probe, Ok(o) if o.status.success()) {
        assert!(
            std::env::var_os("BYTESCAPE_REQUIRE_NAMESPACES").is_none(),
            "BYTESCAPE_REQUIRE_NAMESPACES is set but a user and mount namespace cannot be made"
        );
        eprintln!("NOT RUN: cannot create a user and mount namespace here");
        return;
    }
    let fx = Fixture::new("pin-namespace");
    let exe = std::env::current_exe().unwrap();
    let script = format!(
        "exec '{}' inside_the_namespace_a_pinned_scan_is_not_led_to_another_mount \
         --exact --test-threads=1 --nocapture",
        exe.display()
    );
    let out = Command::new("unshare")
        .args(["--user", "--map-root-user", "--mount", "sh", "-c", &script])
        .env("BS_PIN_DIR", &fx.root)
        .env("RUST_BACKTRACE", "0")
        .output()
        .unwrap();
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "the namespace run failed:\n{text}");
    assert!(
        text.contains("1 passed"),
        "the inner test did not run:\n{text}"
    );
}
