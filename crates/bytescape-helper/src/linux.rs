//! The Linux entry point: who launched this process, and over what.
//!
//! The interface starts the helper with `pkexec`, which asks the user to
//! authenticate and then replaces itself with the helper. The helper's
//! standard input and output are pipes only the interface holds the other ends
//! of, so the connection itself is the proof of who is on the other end. The
//! checks here guard against the helper being started some other way.
//!
//! The only environment variable the helper reads is `PKEXEC_UID`, which
//! `pkexec` sets and cleans.

#![allow(unsafe_code)]

use crate::args;
use crate::delete::Judge;
use crate::scan::Host;
use crate::session::{self, Config, Exit};
use crate::{EXIT_PEER, EXIT_USAGE};
use bs_knowledge::{Environment, Knowledge};
use std::fs::File;
use std::os::fd::{AsFd, AsRawFd};
use std::process::ExitCode;

/// The user id `pkexec` says asked for the elevation. Zero is refused: the
/// interface never runs as the superuser.
fn pkexec_uid() -> Option<u32> {
    parse_pkexec_uid(std::env::var("PKEXEC_UID").ok().as_deref())
}

pub(crate) fn parse_pkexec_uid(value: Option<&str>) -> Option<u32> {
    let text = value?;
    if text.is_empty() || text.len() > 10 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse::<u32>().ok().filter(|uid| *uid != 0)
}

/// The real user id in the text of `/proc/<pid>/status`.
pub(crate) fn real_uid_in_status(status: &str) -> Option<u32> {
    let line = status.lines().find_map(|l| l.strip_prefix("Uid:"))?;
    line.split_whitespace().next()?.parse().ok()
}

/// Whether the process that started this one belongs to `uid`. A parent that
/// has gone (this process was adopted by init) fails: the interface that
/// launched it is no longer there to talk to.
fn parent_belongs_to(uid: u32) -> bool {
    // SAFETY: `getppid` takes no arguments and cannot fail.
    let parent = unsafe { libc::getppid() };
    if parent <= 1 {
        return false;
    }
    std::fs::read_to_string(format!("/proc/{parent}/status"))
        .ok()
        .and_then(|status| real_uid_in_status(&status))
        == Some(uid)
}

/// Whether descriptor `fd` is an anonymous pipe.
fn is_pipe(fd: libc::c_int) -> bool {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
    // SAFETY: `stat` is writable storage of the type `fstat` fills in.
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
        return false;
    }
    // SAFETY: `fstat` succeeded, so the structure is filled in.
    let stat = unsafe { stat.assume_init() };
    stat.st_mode & libc::S_IFMT == libc::S_IFIFO
}

/// Keeps the process out of core dumps and closes every inherited descriptor
/// above the three standard ones.
fn harden() {
    // SAFETY: both calls take plain integers and only change this process's own
    // settings. A failure (an older kernel without `close_range`) leaves the
    // descriptors open, which `pkexec` has already cleaned up.
    unsafe {
        libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
        libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 0u32);
    }
}

/// The home folder of `uid` from the text of `/etc/passwd`.
pub(crate) fn home_in_passwd(text: &str, uid: u32) -> Option<String> {
    text.lines().find_map(|line| {
        let mut fields = line.split(':');
        let _name = fields.next()?;
        let _password = fields.next()?;
        let found: u32 = fields.next()?.parse().ok()?;
        let _gid = fields.next()?;
        let _comment = fields.next()?;
        let home = fields.next()?;
        (found == uid && home.starts_with('/')).then(|| home.to_owned())
    })
}

/// What the interface is told when the account has no home folder on record.
const NO_ACCOUNT: &str = "administrator deletes are not available: this account has no entry in the local account database";

/// The judge for this computer: the protected-path table with the home folder
/// of `uid` from the account database and not from the interface, and every
/// mount point.
///
/// The table protects the user's own folders by name, so it has to know where
/// they are. The account database is read as a plain file: asking the system's
/// name services from an elevated process could reach the network. An account
/// that exists only there has no home on record, and a table without it would
/// protect less than the interface's own does, so such a judge refuses every
/// delete.
fn system_judge(uid: u32, passwd: &std::path::Path, mountinfo: &std::path::Path) -> Judge {
    use std::os::unix::ffi::OsStrExt;
    let mut env = Environment::new();
    let home = std::fs::read_to_string(passwd)
        .ok()
        .and_then(|text| home_in_passwd(&text, uid));
    let has_home = home.is_some();
    if let Some(home) = home {
        env = env.with("HOME", home);
    }
    let mounts = std::fs::read(mountinfo)
        .map(|text| crate::mounts::parse(&text))
        .unwrap_or_default();
    let roots = mounts
        .iter()
        .map(|m| std::path::PathBuf::from(std::ffi::OsStr::from_bytes(&m.mount_point)));
    let judge = Judge::new(Knowledge::new(&env.with_drive_roots(roots)));
    if has_home {
        judge
    } else {
        judge.unavailable(NO_ACCOUNT)
    }
}

pub(crate) fn main() -> ExitCode {
    harden();
    let argv: Vec<_> = std::env::args_os().skip(1).collect();
    let Ok(args) = args::parse(&argv, false) else {
        return ExitCode::from(EXIT_USAGE);
    };
    let Some(uid) = pkexec_uid() else {
        return ExitCode::from(EXIT_PEER);
    };
    if !parent_belongs_to(uid) {
        return ExitCode::from(EXIT_PEER);
    }
    let (stdin, stdout) = (std::io::stdin(), std::io::stdout());
    if !is_pipe(stdin.as_fd().as_raw_fd()) || !is_pipe(stdout.as_fd().as_raw_fd()) {
        return ExitCode::from(EXIT_PEER);
    }
    // Unbuffered handles on the same descriptors.
    let (Ok(input), Ok(output)) = (
        stdin.as_fd().try_clone_to_owned(),
        stdout.as_fd().try_clone_to_owned(),
    ) else {
        return ExitCode::from(EXIT_USAGE);
    };
    let host = Host::system();
    let judge = system_judge(uid, std::path::Path::new("/etc/passwd"), &host.mountinfo);
    let config = Config {
        nonce: args.nonce,
        mode: args.mode,
        request_timeout: session::REQUEST_TIMEOUT,
        write_stall: session::WRITE_STALL,
        judge,
        host,
        on_stall: Box::new(|| std::process::exit(i32::from(Exit::Idle.code()))),
    };
    let exit = session::run(File::from(input), File::from(output), config);
    ExitCode::from(exit.code())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn only_a_plain_number_other_than_zero_names_a_user() {
        assert_eq!(parse_pkexec_uid(Some("1000")), Some(1000));
        assert_eq!(parse_pkexec_uid(Some("4294967295")), Some(u32::MAX));
        for bad in [
            None,
            Some(""),
            Some("0"),
            Some("00"),
            Some("+5"),
            Some("-5"),
            Some("5 "),
            Some("0x10"),
            Some("4294967296"),
            Some("12345678901"),
        ] {
            assert_eq!(parse_pkexec_uid(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_real_uid_is_the_first_number_on_the_uid_line() {
        let status = "Name:\tui\nUmask:\t0022\nUid:\t1000\t1000\t1000\t1000\nGid:\t1000\t1000\n";
        assert_eq!(real_uid_in_status(status), Some(1000));
        // A set-uid process shows its real user first.
        assert_eq!(real_uid_in_status("Uid:\t1000\t0\t0\t0\n"), Some(1000));
        assert_eq!(real_uid_in_status("Gid:\t1\n"), None);
        assert_eq!(real_uid_in_status("Uid:\tabc\n"), None);
        assert_eq!(real_uid_in_status(""), None);
    }

    #[test]
    fn the_home_folder_comes_from_the_matching_account() {
        let passwd = "root:x:0:0:root:/root:/bin/bash\n\
                      alice:x:1000:1000:Alice,,,:/home/alice:/bin/bash\n\
                      bob:x:1001:1001::relative:/bin/sh\n\
                      broken line\n";
        assert_eq!(home_in_passwd(passwd, 1000).as_deref(), Some("/home/alice"));
        assert_eq!(home_in_passwd(passwd, 0).as_deref(), Some("/root"));
        // A home that is not an absolute path is not used.
        assert_eq!(home_in_passwd(passwd, 1001), None);
        assert_eq!(home_in_passwd(passwd, 4242), None);
    }

    #[test]
    fn an_account_with_no_passwd_entry_gets_a_judge_that_refuses_every_delete() {
        let f = crate::testutil::Fixture::new("no-account");
        f.write(
            "passwd",
            "root:x:0:0:root:/root:/bin/bash\nalice:x:1000:1000::/home/alice:/bin/sh\n",
        );
        f.write("mountinfo", "");
        let passwd = f.p("passwd");
        let mountinfo = f.p("mountinfo");
        // Known account: the table is complete, so deletes are judged as usual.
        assert_eq!(system_judge(1000, &passwd, &mountinfo).refusal(), None);
        // Every way the account file can fail to name the user's home: the user
        // exists only through the name services, the file is missing, it
        // cannot be read (here a folder stands in for it), or it is not text
        // (a file with a byte that is not UTF-8 is refused as a whole, even for
        // a user whose own line is fine). Each is refused, with a reason the
        // interface can show.
        f.mkdir("unreadable");
        std::fs::write(
            f.p("binary"),
            b"alice:x:1000:1000:Ali\xe7e:/home/alice:/bin/sh\n",
        )
        .unwrap();
        for (uid, file) in [
            (4242, passwd.clone()),
            (1000, f.p("absent")),
            (1000, f.p("unreadable")),
            (1000, f.p("binary")),
        ] {
            let judge = system_judge(uid, &file, &mountinfo);
            let why = judge.refusal().expect("deletes are refused");
            assert!(why.contains("account"), "{why}");
        }
    }
}
