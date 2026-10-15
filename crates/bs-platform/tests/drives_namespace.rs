//! The drive list on a real mount table with a subvolume-shaped layout: a file
//! system with a folder `home` mounted on that very folder, and a folder of it
//! mounted on its own folder in turn. The layout is made with bind mounts of one
//! tmpfs, which gives the kernel's table the same shape a btrfs top level with
//! `home` on `/home` has (one device number, roots that are folders of the file
//! system, mount points on those folders). A real btrfs cannot be mounted
//! without being the superuser of the machine.
//!
//! This needs the right to mount, so it runs in a private user and mount
//! namespace (`unshare --user --map-root-user --mount`) or as the superuser on
//! a machine that may be changed. Where it cannot mount it prints `NOT RUN`
//! and passes, unless `BYTESCAPE_REQUIRE_NAMESPACES` is set, which makes that a
//! failure.
#![cfg(target_os = "linux")]
#![allow(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

fn c(path: &Path) -> CString {
    CString::new(path.as_os_str().as_bytes()).unwrap()
}

/// A mount that is detached when it goes out of scope.
struct Mounted(PathBuf);

impl Drop for Mounted {
    fn drop(&mut self) {
        // SAFETY: a NUL-terminated path; detaching a mount this test made.
        unsafe { libc::umount2(c(&self.0).as_ptr(), libc::MNT_DETACH) };
    }
}

fn mount_tmpfs(target: &Path) -> Option<Mounted> {
    // SAFETY: all strings are NUL-terminated and outlive the call; no data.
    let status = unsafe {
        libc::mount(
            c"tmpfs".as_ptr(),
            c(target).as_ptr(),
            c"tmpfs".as_ptr(),
            0,
            std::ptr::null(),
        )
    };
    (status == 0).then(|| Mounted(target.to_path_buf()))
}

fn bind_mount(source: &Path, target: &Path) -> Option<Mounted> {
    // SAFETY: all strings are NUL-terminated and outlive the call; no data.
    let status = unsafe {
        libc::mount(
            c(source).as_ptr(),
            c(target).as_ptr(),
            std::ptr::null(),
            libc::MS_BIND,
            std::ptr::null(),
        )
    };
    (status == 0).then(|| Mounted(target.to_path_buf()))
}

struct Fixture(PathBuf);

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn not_run(why: &str) {
    assert!(
        std::env::var_os("BYTESCAPE_REQUIRE_NAMESPACES").is_none(),
        "BYTESCAPE_REQUIRE_NAMESPACES is set but {why}"
    );
    println!("NOT RUN: {why}");
}

#[test]
fn a_mount_on_its_own_folder_is_not_covered_on_a_real_mount_table() {
    let base = std::env::temp_dir().join(format!("bs-drives-ns-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("top")).unwrap();
    let base = std::fs::canonicalize(&base).unwrap();
    let _fixture = Fixture(base.clone());
    let top = base.join("top");
    let Some(_top) = mount_tmpfs(&top) else {
        return not_run("a tmpfs cannot be mounted here");
    };
    std::fs::create_dir_all(top.join("home/alice")).unwrap();
    std::fs::create_dir_all(top.join("srv")).unwrap();
    std::fs::create_dir_all(top.join("elsewhere")).unwrap();
    let Some(_home) = bind_mount(&top.join("home"), &top.join("home")) else {
        return not_run("a bind mount cannot be made here");
    };
    let _alice = bind_mount(&top.join("home/alice"), &top.join("home/alice")).unwrap();
    // A second view of `home` on another folder: a repeat, which a scan of the
    // top must not count again.
    let _again = bind_mount(&top.join("home"), &top.join("elsewhere")).unwrap();

    let drive = bs_platform::drives::volume_for_path(&top).unwrap();
    let covered = |relative: &str| {
        let at = top.join(relative);
        drive
            .mounts
            .iter()
            .find(|m| m.mount_point == at)
            .unwrap_or_else(|| panic!("no mount at {} in {:?}", at.display(), drive.mounts))
            .covered_by_other_mount
    };
    assert!(!covered(""), "the start covered by something");
    assert!(!covered("home"), "/home became covered by the top level");
    assert!(
        !covered("home/alice"),
        "/home/alice became covered by the top level or by /home"
    );
    assert!(covered("elsewhere"), "a second view of /home is a repeat");
}
