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
    Backend, Client, ClientError, DeleteItem, DeleteRequest, ErrorCode, ItemOutcome, Refusal,
    ScanEnd, ScanEvent, ScanRequest, path_to_wire,
};
use common::{Fixture, NONCE, NONCE_HEX, helper_path};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// An ordinary account the helper acts for: its ids and home folder, read from
/// the account file.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Ordinary {
    uid: u32,
    gid: u32,
    home: Vec<u8>,
}

/// The account lines of `passwd` as (name, account), skipping lines that are
/// not in the usual seven-field shape or whose home is not an absolute path.
fn accounts_in(passwd: &[u8]) -> Vec<(Vec<u8>, Ordinary)> {
    let mut found = Vec::new();
    for line in passwd.split(|&b| b == b'\n') {
        let fields: Vec<&[u8]> = line.split(|&b| b == b':').collect();
        let number = |i: usize| {
            std::str::from_utf8(fields.get(i)?)
                .ok()?
                .parse::<u32>()
                .ok()
        };
        let (Some(uid), Some(gid), Some(home)) = (number(2), number(3), fields.get(5)) else {
            continue;
        };
        if home.first() == Some(&b'/') && !home.contains(&0) {
            found.push((
                fields[0].to_vec(),
                Ordinary {
                    uid,
                    gid,
                    home: home.to_vec(),
                },
            ));
        }
    }
    found
}

/// The ordinary account to act for: `nobody` when the system has it, else the
/// first other account that is not root. Its home must be a real folder name
/// (not `/`) that the fixture is not inside: the helper protects a user's own
/// home, and a fixture in it would be refused for that reason, not the one a
/// test is about.
fn ordinary_account(passwd: &[u8], fixture: &Path) -> Option<Ordinary> {
    use std::os::unix::ffi::OsStrExt;
    let usable = |account: &Ordinary| {
        account.uid != 0
            && account.home != b"/"
            && !fixture
                .as_os_str()
                .as_bytes()
                .starts_with(&[account.home.as_slice(), b"/"].concat())
    };
    let all = accounts_in(passwd);
    all.iter()
        .find(|(name, a)| name == b"nobody" && usable(a))
        .or_else(|| all.iter().find(|(_, a)| usable(a)))
        .map(|(_, a)| a.clone())
}

/// A user id that no line of `passwd` names, for the account-less case.
fn unlisted_uid(passwd: &[u8]) -> u32 {
    let taken: Vec<u32> = accounts_in(passwd).iter().map(|(_, a)| a.uid).collect();
    // Lines whose home is unusable still name their id, so read every id.
    let every: Vec<u32> = passwd
        .split(|&b| b == b'\n')
        .filter_map(|line| {
            let id = line.split(|&b| b == b':').nth(2)?;
            std::str::from_utf8(id).ok()?.parse().ok()
        })
        .collect();
    (31_337u32..)
        .find(|uid| !taken.contains(uid) && !every.contains(uid))
        .unwrap()
}

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
    user: Ordinary,
}

/// Prepares the stand-in for an account that is in the account file, or says
/// why this machine cannot run these tests.
fn setup(label: &str) -> Result<Root, String> {
    setup_for(label, |passwd, fixture| {
        ordinary_account(passwd, fixture)
            .ok_or_else(|| "no ordinary account in /etc/passwd to act for".to_string())
    })
}

/// Prepares the stand-in for an account that has no line in the account file,
/// as one known only to a network directory has none.
fn setup_unlisted(label: &str) -> Result<Root, String> {
    setup_for(label, |passwd, _| {
        let uid = unlisted_uid(passwd);
        Ok(Ordinary {
            uid,
            gid: uid,
            home: b"/home/unlisted".to_vec(),
        })
    })
}

fn setup_for(
    label: &str,
    choose: impl FnOnce(&[u8], &Path) -> Result<Ordinary, String>,
) -> Result<Root, String> {
    if std::env::var("BYTESCAPE_DISPOSABLE_HOST").as_deref() != Ok("1") {
        return Err("BYTESCAPE_DISPOSABLE_HOST=1 is not set".into());
    }
    if effective_uid() != 0 {
        return Err("not running as the superuser".into());
    }
    let fx = Fixture::new(label);
    let passwd = fs::read("/etc/passwd").map_err(|e| format!("no /etc/passwd: {e}"))?;
    let user = choose(&passwd, fx.path())?;
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
    close_to_everyone_but(su_env.parent().unwrap(), user.uid, user.gid)
        .map_err(|e| e.to_string())?;
    // A file system mounted `nosuid` ignores the bit.
    let name = std::ffi::CString::new(su_env.to_str().unwrap()).unwrap();
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
    // SAFETY: `name` is NUL-terminated and `stat` is writable storage.
    let ok = unsafe { libc::statvfs(name.as_ptr(), stat.as_mut_ptr()) } == 0;
    // SAFETY: filled in by the successful call above.
    if !ok || unsafe { stat.assume_init() }.f_flag & libc::ST_NOSUID != 0 {
        return Err("the temporary folder is mounted nosuid".into());
    }
    Ok(Root { fx, su_env, user })
}

macro_rules! root_or_return {
    ($label:expr) => {
        root_or_return!($label, setup)
    };
    ($label:expr, $setup:ident) => {
        match $setup($label) {
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
            "\"$0\" PKEXEC_UID={} \"$1\" {mode} --protocol 1 --nonce {NONCE_HEX}; exit $?",
            self.user.uid
        );
        let mut child = Command::new("/bin/sh")
            .uid(self.user.uid)
            .gid(self.user.gid)
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
            .uid(self.user.uid)
            .gid(self.user.gid)
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
fn an_account_with_no_passwd_entry_gets_no_delete_and_nothing_is_removed() {
    let root = root_or_return!("root-no-account", setup_unlisted);
    let fx = &root.fx;
    fx.write("victim/inner/f", "x");
    let (mut child, mut client) = root.client("delete");
    let failure = client
        .delete(DeleteRequest {
            permanent: true,
            items: vec![DeleteItem {
                path: path_to_wire(&fx.p("victim/inner")),
                confirmed_protected: false,
                seen: None,
            }],
        })
        .unwrap_err();
    assert!(
        matches!(
            &failure.error,
            ClientError::Helper(report) if report.code == ErrorCode::NoAccount
        ),
        "{failure:?}"
    );
    assert!(failure.completed.is_empty(), "{failure:?}");
    assert!(fx.exists("victim/inner/f"), "nothing may be deleted");
    drop(client);
    child.wait().unwrap();
}

#[test]
fn the_helper_refuses_a_parent_that_is_not_the_user_it_was_told_about() {
    let root = root_or_return!("root-parent");
    // The shell belongs to the ordinary user, but PKEXEC_UID names someone else.
    let script = format!(
        "\"$0\" PKEXEC_UID={} \"$1\" scan --protocol 1 --nonce {NONCE_HEX}; exit $?",
        root.user.uid + 1
    );
    let status = Command::new("/bin/sh")
        .uid(root.user.uid)
        .gid(root.user.gid)
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
        .uid(root.user.uid)
        .gid(root.user.gid)
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

// ------------------------------------------- choosing the account

#[test]
fn the_account_to_act_for_is_nobody_when_there_is_one_and_never_root() {
    let without_nobody = b"root:x:0:0:root:/root:/bin/bash\n\
alice:x:1000:1000::/home/alice:/bin/sh\n";
    let passwd = [
        &without_nobody[..],
        b"nobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin\n",
    ]
    .concat();
    let fixture = Path::new("/tmp/bs-helper-it-x");
    let chosen = ordinary_account(&passwd, fixture).unwrap();
    assert_eq!((chosen.uid, chosen.gid), (65534, 65534));
    assert_eq!(chosen.home, b"/nonexistent");
    // Without nobody, the first ordinary account; root alone is no choice.
    let chosen = ordinary_account(without_nobody, fixture).unwrap();
    assert_eq!(chosen.uid, 1000);
    assert_eq!(
        ordinary_account(b"root:x:0:0:root:/root:/bin/bash\n", fixture),
        None
    );
}

#[test]
fn an_account_whose_home_holds_the_fixture_or_is_the_top_is_not_chosen() {
    let passwd = b"alice:x:1000:1000::/home/alice:/bin/sh\n\
bob:x:1001:1001::/home/bob:/bin/sh\n\
sys:x:3:3::/:/bin/sh\n\
odd:x:4:4::relative:/bin/sh\n";
    let inside_alice = Path::new("/home/alice/tmp/bs-helper-it-x");
    assert_eq!(ordinary_account(passwd, inside_alice).unwrap().uid, 1001);
    // A home that only shares a name prefix does not hold the fixture.
    let beside = Path::new("/home/alice2/bs-helper-it-x");
    assert_eq!(ordinary_account(passwd, beside).unwrap().uid, 1000);
}

#[test]
fn the_account_less_user_id_is_one_no_line_names() {
    let passwd = b"a:x:31337:1::/home/a:/bin/sh\nb:x:31338:1::relative:/bin/sh\nbroken\n";
    // Both 31337 and 31338 are named, the second by a line with a bad home.
    assert_eq!(unlisted_uid(passwd), 31_339);
    assert_eq!(unlisted_uid(b""), 31_337);
}
