//! Deleting around other mounts. This needs the right to mount, so it runs in
//! a private user and mount namespace (`unshare --user --map-root-user
//! --mount`) or as the superuser on a machine that may be changed. Where it
//! cannot mount it prints `NOT RUN` and passes, and the script that runs the
//! privileged tests fails on that text.
//!
//! The mounts are tmpfs and bind mounts inside a fixture folder. They are
//! detached again when the test ends.

#![allow(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
#![cfg(target_os = "linux")]

mod common;

use bs_ipc::{DeleteItem, DeleteRequest, ItemOutcome, Refusal, path_to_wire};
use bs_knowledge::{Environment, Host, Knowledge};
use bytescape_helper::delete::Judge;
use common::Fixture;
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// A mount that is detached when it goes out of scope.
struct Mounted(PathBuf);

impl Drop for Mounted {
    fn drop(&mut self) {
        let target = CString::new(self.0.as_os_str().as_bytes()).unwrap();
        // SAFETY: `target` is NUL-terminated; detaching a mount made by this test.
        unsafe { libc::umount2(target.as_ptr(), libc::MNT_DETACH) };
    }
}

fn c(path: &Path) -> CString {
    CString::new(path.as_os_str().as_bytes()).unwrap()
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

fn judge() -> Judge {
    Judge::new(Knowledge::for_host(&Environment::new(), Host::Linux))
}

fn request(paths: &[&Path]) -> DeleteRequest {
    DeleteRequest {
        permanent: true,
        items: paths
            .iter()
            .map(|p| DeleteItem {
                path: path_to_wire(p),
                confirmed_protected: false,
                seen: None,
            })
            .collect(),
    }
}

fn run(request: &DeleteRequest) -> Vec<ItemOutcome> {
    let mut out = Vec::new();
    judge().run(
        request,
        || true,
        |_, outcome| {
            out.push(outcome);
            true
        },
    );
    out
}

fn not_run(why: &str) {
    println!("NOT RUN: {why}");
}

#[test]
fn a_mount_point_is_never_deleted_and_nothing_behind_it_is_touched() {
    let fx = Fixture::new("mount-point");
    fx.write("outer/own-file", "x");
    fx.mkdir("outer/mnt");
    let Some(_tmpfs) = mount_tmpfs(&fx.p("outer/mnt")) else {
        return not_run("cannot mount a tmpfs here");
    };
    fx.write("outer/mnt/inside", "behind the mount");

    // The mount point itself.
    let outcomes = run(&request(&[&fx.p("outer/mnt")]));
    assert_eq!(outcomes[0], ItemOutcome::Refused(Refusal::MountPoint));
    assert!(fx.exists("outer/mnt/inside"));

    // The folder that holds it: its own files go, the other filesystem is left
    // alone, and the result says the delete did not finish.
    let outcomes = run(&request(&[&fx.p("outer")]));
    match outcomes[0] {
        ItemOutcome::Failed {
            step,
            removed_entries,
            ..
        } => {
            assert_eq!(step, 2);
            assert!(removed_entries >= 1);
        }
        other => panic!("a delete that stopped at a mount must not look complete: {other:?}"),
    }
    assert!(!fx.exists("outer/own-file"));
    assert!(fx.exists("outer/mnt/inside"));
}

#[test]
fn a_bind_mount_of_the_same_filesystem_is_not_followed_either() {
    let fx = Fixture::new("bind-mount");
    fx.write("elsewhere/keep", "x");
    fx.write("outer/own-file", "x");
    fx.mkdir("outer/bound");
    let Some(_bound) = bind_mount(&fx.p("elsewhere"), &fx.p("outer/bound")) else {
        return not_run("cannot make a bind mount here");
    };
    assert!(fx.exists("outer/bound/keep"));

    let outcomes = run(&request(&[&fx.p("outer/bound")]));
    assert_eq!(outcomes[0], ItemOutcome::Refused(Refusal::MountPoint));
    assert!(fx.exists("elsewhere/keep"));

    let outcomes = run(&request(&[&fx.p("outer")]));
    assert!(
        matches!(outcomes[0], ItemOutcome::Failed { step: 2, .. }),
        "{:?}",
        outcomes[0]
    );
    // The same data under another name is the same data: it must still be there.
    assert!(fx.exists("elsewhere/keep"));
    assert!(fx.exists("outer/bound/keep"));
}

#[test]
fn an_ordinary_folder_next_to_a_mount_is_deleted_whole() {
    let fx = Fixture::new("beside-mount");
    fx.mkdir("mnt");
    let Some(_tmpfs) = mount_tmpfs(&fx.p("mnt")) else {
        return not_run("cannot mount a tmpfs here");
    };
    fx.write("plain/a/b", "x");
    let outcomes = run(&request(&[&fx.p("plain")]));
    assert!(matches!(
        outcomes[0],
        ItemOutcome::Deleted { entries: 3, .. }
    ));
    assert!(!fx.exists("plain"));
}

#[test]
fn a_mounted_tmpfs_is_found_in_the_mount_table() {
    // The reader of the mount table finds a mount made just now by its exact
    // place, which is how the raw reader and the drive roots are decided.
    let fx = Fixture::new("table");
    fx.mkdir("mnt");
    let Some(_tmpfs) = mount_tmpfs(&fx.p("mnt")) else {
        return not_run("cannot mount a tmpfs here");
    };
    let table = fs::read("/proc/self/mountinfo").unwrap();
    let mounts = bytescape_helper::mounts::parse(&table);
    let here = bytescape_helper::mounts::at_exactly(&mounts, fx.p("mnt").as_os_str().as_bytes());
    assert_eq!(here.map(|m| m.fs_type.as_str()), Some("tmpfs"));
}
