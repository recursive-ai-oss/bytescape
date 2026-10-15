//! The helper program with the superuser's rights, on a machine that may be
//! changed (`BYTESCAPE_DISPOSABLE_HOST=1`, running as root). Where it cannot
//! run it prints `NOT RUN` and passes, and the script that runs the privileged
//! tests fails on that text.
//!
//! `pkexec` cannot be used unattended, so a stand-in does its part: a copy of
//! `env` that is set-user-id root, started by a shell of an ordinary user. That
//! gives the helper what `pkexec` gives it: the rights of root, a parent
//! process that belongs to the ordinary user, and `PKEXEC_UID` naming that
//! user. Everything the helper does is its own code; the password prompt is
//! the only thing not tried.
//!
//! All files are made in a fixture folder and removed afterwards.

#![allow(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
#![allow(clippy::disallowed_types, clippy::disallowed_methods)]
#![cfg(target_os = "linux")]

mod common;

use bs_ipc::{
    Backend, Client, DeleteItem, DeleteRequest, ItemOutcome, Refusal, ScanEnd, ScanEvent,
    ScanRequest, path_to_wire,
};
use common::{Fixture, NONCE, NONCE_HEX, helper_path};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// The ordinary user the helper acts for. It need not exist in the account
/// database.
const USER: u32 = 31_337;

fn effective_uid() -> u32 {
    fs::metadata("/proc/self").unwrap().uid()
}

/// Checks that every folder above `path` can be searched by anyone, which the
/// ordinary user needs to reach a file inside. Nothing is changed.
fn searchable_above(path: &Path) -> Result<(), String> {
    let mut dir = path.parent();
    while let Some(d) = dir {
        if d == Path::new("/") {
            break;
        }
        let mode = fs::metadata(d)
            .map_err(|e| e.to_string())?
            .permissions()
            .mode();
        if mode & 0o001 == 0 {
            return Err(format!(
                "{} cannot be searched by an ordinary user; use a temporary folder that can (TMPDIR)",
                d.display()
            ));
        }
        dir = d.parent();
    }
    Ok(())
}

/// Makes `dir` belong to `uid` and `gid` and closes it to everyone else.
fn close_to_everyone_but(dir: &Path, uid: u32, gid: u32) -> std::io::Result<()> {
    std::os::unix::fs::chown(dir, Some(uid), Some(gid))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

struct Root {
    fx: Fixture,
    su_env: std::path::PathBuf,
}

/// Prepares the stand-in, or says why this machine cannot run these tests.
fn setup(label: &str) -> Result<Root, String> {
    if std::env::var("BYTESCAPE_DISPOSABLE_HOST").as_deref() != Ok("1") {
        return Err("BYTESCAPE_DISPOSABLE_HOST=1 is not set".into());
    }
    if effective_uid() != 0 {
        return Err("not running as the superuser".into());
    }
    let fx = Fixture::new(label);
    // The folder made for this test is ours to open up. The folders above it
    // are not: the ordinary user must be able to search them already, and if
    // not the test says so and does not run, instead of changing them.
    fs::set_permissions(fx.path(), fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    searchable_above(fx.path())?;
    let su_env = fx.p("su/env");
    fs::create_dir_all(su_env.parent().unwrap()).map_err(|e| e.to_string())?;
    fs::copy("/usr/bin/env", &su_env).map_err(|e| format!("no /usr/bin/env: {e}"))?;
    fs::set_permissions(&su_env, fs::Permissions::from_mode(0o4755)).map_err(|e| e.to_string())?;
    // Only the ordinary user the stand-in acts for can reach it: a copy of
    // `env` that is set-user-id root would give anyone who could start it a
    // root shell, for as long as it exists.
    close_to_everyone_but(su_env.parent().unwrap(), USER, USER).map_err(|e| e.to_string())?;
    // A file system mounted `nosuid` ignores the bit.
    let name = std::ffi::CString::new(su_env.to_str().unwrap()).unwrap();
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
    // SAFETY: `name` is NUL-terminated and `stat` is writable storage.
    let ok = unsafe { libc::statvfs(name.as_ptr(), stat.as_mut_ptr()) } == 0;
    // SAFETY: filled in by the successful call above.
    if !ok || unsafe { stat.assume_init() }.f_flag & libc::ST_NOSUID != 0 {
        return Err("the temporary folder is mounted nosuid".into());
    }
    Ok(Root { fx, su_env })
}

macro_rules! root_or_return {
    ($label:expr) => {
        match setup($label) {
            Ok(root) => root,
            Err(why) => {
                println!("NOT RUN: {why}");
                return;
            }
        }
    };
}

impl Root {
    /// Starts the helper as `pkexec` would: root's rights, a parent that is the
    /// ordinary user's shell, `PKEXEC_UID` naming that user.
    fn start(&self, mode: &str) -> (Child, ChildStdin, ChildStdout) {
        let script = format!(
            "\"$0\" PKEXEC_UID={USER} \"$1\" {mode} --protocol 1 --nonce {NONCE_HEX}; exit $?"
        );
        let mut child = Command::new("/bin/sh")
            .uid(USER)
            .gid(USER)
            .arg("-c")
            .arg(script)
            .arg(&self.su_env)
            .arg(helper_path())
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        (child, stdin, stdout)
    }

    fn client(&self, mode: &str) -> (Child, Client<ChildStdout, ChildStdin>) {
        let (child, stdin, stdout) = self.start(mode);
        (child, Client::connect(stdout, stdin, NONCE).unwrap())
    }

    /// Whether the ordinary user can open `path` as a folder.
    fn user_can_list(&self, path: &Path) -> bool {
        Command::new("/bin/ls")
            .uid(USER)
            .gid(USER)
            .arg(path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    }
}

#[test]
fn a_scan_as_root_reads_what_the_user_cannot_open() {
    let root = root_or_return!("root-scan");
    let fx = &root.fx;
    fx.write("tree/open/a", "a");
    fx.write("tree/secret/inner/b", "bb");
    fs::set_permissions(fx.p("tree/secret"), fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        !root.user_can_list(&fx.p("tree/secret")),
        "the folder must be closed to the user"
    );

    let (mut child, mut client) = root.client("scan");
    let mut stream = client
        .scan(ScanRequest {
            volume: path_to_wire(&fx.p("tree")),
            backend: Backend::Walker,
            extra_mount_ids: vec![],
            places: vec![],
        })
        .unwrap();
    let mut batches = Vec::new();
    let mut done = None;
    while let Some(event) = stream.next_event().unwrap() {
        match event {
            ScanEvent::Batch(b) => batches.push(b),
            ScanEvent::Done(d) => done = Some(d),
            _ => {}
        }
    }
    let done = done.unwrap();
    assert_eq!(done.end, ScanEnd::Completed);
    assert_eq!(done.progress.unreadable, 0, "nothing was closed to root");
    assert_eq!(
        done.progress.files, 2,
        "both files, including the one in the closed folder"
    );
    drop(client);
    assert!(child.wait().unwrap().success());
}

#[test]
fn a_delete_as_root_removes_what_the_user_cannot() {
    let root = root_or_return!("root-delete");
    let fx = &root.fx;
    fx.write("closed/inner/f", "x");
    fs::set_permissions(fx.p("closed"), fs::Permissions::from_mode(0o700)).unwrap();
    fx.write("open/keep", "k");
    assert!(!root.user_can_list(&fx.p("closed")));

    let (mut child, mut client) = root.client("delete");
    let outcomes = client
        .delete(DeleteRequest {
            permanent: true,
            items: vec![DeleteItem {
                path: path_to_wire(&fx.p("closed/inner")),
                confirmed_protected: false,
                seen: None,
            }],
        })
        .unwrap();
    assert!(
        matches!(outcomes[0], ItemOutcome::Deleted { entries: 2, .. }),
        "{outcomes:?}"
    );
    assert!(!fx.exists("closed/inner"));
    assert!(fx.exists("open/keep"));
    drop(client);
    assert!(child.wait().unwrap().success());
}

#[test]
fn as_root_the_operating_systems_places_are_still_refused() {
    let root = root_or_return!("root-system");
    // Places that do not exist, so even a mistake would have nothing to delete.
    let paths = [
        "/usr/bytescape-test-absent",
        "/etc/bytescape-test-absent",
        "/boot/bytescape-test-absent",
        "/var/lib/dpkg/bytescape-test-absent",
        "/proc/bytescape-test-absent",
    ];
    for path in paths {
        assert!(fs::symlink_metadata(path).is_err(), "{path} must not exist");
    }
    let (mut child, mut client) = root.client("delete");
    let outcomes = client
        .delete(DeleteRequest {
            permanent: true,
            items: paths
                .iter()
                .map(|p| DeleteItem {
                    path: path_to_wire(Path::new(p)),
                    confirmed_protected: true,
                    seen: None,
                })
                .collect(),
        })
        .unwrap();
    for outcome in outcomes {
        assert_eq!(outcome, ItemOutcome::Refused(Refusal::Blocked));
    }
    drop(client);
    assert!(child.wait().unwrap().success());
}

#[test]
fn the_helper_refuses_a_parent_that_is_not_the_user_it_was_told_about() {
    let root = root_or_return!("root-parent");
    // The shell belongs to USER, but PKEXEC_UID names someone else.
    let script = format!(
        "\"$0\" PKEXEC_UID={} \"$1\" scan --protocol 1 --nonce {NONCE_HEX}; exit $?",
        USER + 1
    );
    let status = Command::new("/bin/sh")
        .uid(USER)
        .gid(USER)
        .arg("-c")
        .arg(script)
        .arg(&root.su_env)
        .arg(helper_path())
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(3));
    // And the superuser is never an acceptable user to act for.
    let script =
        format!("\"$0\" PKEXEC_UID=0 \"$1\" scan --protocol 1 --nonce {NONCE_HEX}; exit $?");
    let status = Command::new("/bin/sh")
        .uid(USER)
        .gid(USER)
        .arg("-c")
        .arg(script)
        .arg(&root.su_env)
        .arg(helper_path())
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(3));
}

// ------------------------------------------- the stand-in's own safety

#[test]
fn the_stand_in_folder_is_closed_to_everyone_but_the_user_it_acts_for() {
    // Runs without root: the folder is given to its current owner, which is
    // what changing the owner to the ordinary user does for the real run.
    let fx = Fixture::new("stand-in-closed");
    fx.mkdir("su");
    fs::set_permissions(fx.p("su"), fs::Permissions::from_mode(0o755)).unwrap();
    let before = fs::metadata(fx.p("su")).unwrap();
    close_to_everyone_but(&fx.p("su"), before.uid(), before.gid()).unwrap();
    let after = fs::metadata(fx.p("su")).unwrap();
    assert_eq!(after.permissions().mode() & 0o777, 0o700);
    assert_eq!((after.uid(), after.gid()), (before.uid(), before.gid()));
}

#[test]
fn the_folders_above_the_stand_in_are_checked_and_never_changed() {
    let fx = Fixture::new("stand-in-above");
    fx.mkdir("a/b");
    let closed = fx.p("a");
    fs::set_permissions(&closed, fs::Permissions::from_mode(0o700)).unwrap();
    let before = fs::metadata(&closed).unwrap().permissions().mode();
    let error = searchable_above(&fx.p("a/b/c")).unwrap_err();
    assert!(error.contains("cannot be searched"), "{error}");
    assert!(error.contains("TMPDIR"), "{error}");
    // The check changed nothing.
    assert_eq!(fs::metadata(&closed).unwrap().permissions().mode(), before);
    fs::set_permissions(&closed, fs::Permissions::from_mode(0o755)).unwrap();
    searchable_above(&fx.p("a/b/c")).unwrap();
}
