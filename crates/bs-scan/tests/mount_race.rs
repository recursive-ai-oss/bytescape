//! A mount placed on a directory after it was listed but before it was opened
//! must not be entered: the opened directory is checked against the boundary,
//! not only the entry that was examined by name.
//!
//! The mount is made by a hook at exactly that moment, inside a private user
//! and mount namespace (`unshare --user --map-root-user --mount`), so it
//! needs no privilege on the host. Where the namespace cannot be made the
//! test prints `NOT RUN` and returns. One inner test per file because the
//! hook is process-wide.
#![cfg(target_os = "linux")]

mod support;

use bs_tree::ReadErrorKind;
use std::process::Command;
use support::*;

/// Runs inside the namespace; outside it (a plain `cargo test`) it has
/// nothing to do.
#[test]
fn inside_the_namespace_a_mount_placed_after_listing_is_refused() {
    let Some(dir) = std::env::var_os("BS_RACE_DIR") else {
        return;
    };
    let root = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(root.join("victim")).unwrap();
    std::fs::write(root.join("victim/inner.txt"), vec![b'x'; 100]).unwrap();
    std::fs::create_dir_all(root.join("bystander")).unwrap();
    std::fs::write(root.join("bystander/ok.txt"), vec![b'x'; 200]).unwrap();

    let victim = root.join("victim");
    bs_scan::set_before_open_hook(Some(Box::new(move |name| {
        if name == b"victim" {
            let status = Command::new("mount")
                .args(["-t", "tmpfs", "-o", "size=8m", "none"])
                .arg(&victim)
                .status()
                .unwrap();
            assert!(status.success(), "mount failed");
            std::fs::write(victim.join("other-drive.bin"), vec![b'y'; 1 << 20]).unwrap();
        }
    })));
    let result = scan(&root);
    bs_scan::set_before_open_hook(None);

    result.snapshot.verify().unwrap();
    let snap = &result.snapshot;
    assert!(snap.scan_complete());
    assert!(
        snap.root().unwrap().logical() < (1 << 19),
        "the mounted filesystem was entered and counted"
    );
    let victim = node_at(snap, "victim");
    assert!(victim.is_unreadable());
    assert_eq!(victim.read_failure().unwrap().kind, ReadErrorKind::NotFound);
    assert_eq!(victim.children().count(), 0);
    assert!(snap.root().unwrap().bytes_are_lower_bound());
    assert_eq!(node_at(snap, "bystander/ok.txt").logical(), 200);
}

#[test]
fn a_mount_placed_after_listing_is_refused_in_a_private_namespace() {
    if std::env::var_os("BS_RACE_DIR").is_some() {
        // This is the inner run; the other test does the work.
        return;
    }
    let probe = Command::new("unshare")
        .args(["--user", "--map-root-user", "--mount", "true"])
        .output();
    if !matches!(&probe, Ok(o) if o.status.success()) {
        eprintln!("NOT RUN: cannot create a user and mount namespace here");
        return;
    }
    let fx = Fixture::new("mount-race");
    let exe = std::env::current_exe().unwrap();
    let script = format!(
        "exec '{}' inside_the_namespace_a_mount_placed_after_listing_is_refused \
         --exact --test-threads=1 --nocapture",
        exe.display()
    );
    let out = Command::new("unshare")
        .args(["--user", "--map-root-user", "--mount", "sh", "-c", &script])
        .env("BS_RACE_DIR", &fx.root)
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
