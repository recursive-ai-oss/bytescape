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
use bs_ipc::ErrorCode;
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

/// What the account file says about one user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Account {
    /// The user's home folder, as the bytes of the file give it.
    Home(Vec<u8>),
    /// The user has a line, but it does not name an absolute home folder.
    Damaged,
    /// No line has this user id.
    Missing,
}

/// What `uid` is in the bytes of `/etc/passwd`: the first line that names it,
/// as the system's own lookup would pick. Only that line is read as text. The
/// file may hold names and comments in any encoding, so the other lines are
/// split on bytes and never need to be valid UTF-8; a line whose user id
/// field cannot be read as a number belongs to nobody.
pub(crate) fn account_in_passwd(text: &[u8], uid: u32) -> Account {
    for line in text.split(|&b| b == b'\n') {
        let fields: Vec<&[u8]> = line.split(|&b| b == b':').collect();
        let named = fields
            .get(2)
            .filter(|id| !id.is_empty() && id.iter().all(u8::is_ascii_digit))
            .and_then(|id| std::str::from_utf8(id).ok())
            .and_then(|id| id.parse::<u32>().ok());
        if named != Some(uid) {
            continue;
        }
        return match fields.get(5) {
            Some(home) if home.first() == Some(&b'/') && !home.contains(&0) => {
                Account::Home((*home).to_vec())
            }
            _ => Account::Damaged,
        };
    }
    Account::Missing
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
/// delete. Only the user's own line matters: damage elsewhere in the file is
/// no reason to refuse.
fn system_judge(uid: u32, passwd: &std::path::Path, mountinfo: &std::path::Path) -> Judge {
    use std::os::unix::ffi::OsStrExt;
    let mut env = Environment::new();
    // A file that cannot be read has no line for the user either.
    let account =
        std::fs::read(passwd).map_or(Account::Missing, |text| account_in_passwd(&text, uid));
    let has_home = if let Account::Home(home) = account {
        env = env.with("HOME", home);
        true
    } else {
        false
    };
    let mounts = std::fs::read(mountinfo)
        .map(|text| crate::mounts::parse(&text))
        .unwrap_or_default();
    let roots = mounts
        .iter()
        .map(|m| std::path::PathBuf::from(std::ffi::OsStr::from_bytes(&m.mount_point)));
    let judge = Judge::new(Knowledge::new(&env.with_drive_roots(roots)))
        .with_volume_check(Box::new(crate::volume::MountTable::new(mountinfo)));
    if has_home {
        judge
    } else {
        judge.unavailable(ErrorCode::NoAccount, NO_ACCOUNT)
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
        let passwd = b"root:x:0:0:root:/root:/bin/bash\n\
                       alice:x:1000:1000:Alice,,,:/home/alice:/bin/bash\n\
                       bob:x:1001:1001::relative:/bin/sh\n\
                       carol:x:1002:1002::/home/carol\n\
                       dave:x:1003:1003:::/bin/sh\n\
                       broken line\n";
        assert_eq!(
            account_in_passwd(passwd, 1000),
            Account::Home(b"/home/alice".to_vec())
        );
        assert_eq!(
            account_in_passwd(passwd, 0),
            Account::Home(b"/root".to_vec())
        );
        // A line without a shell field is still a line with a home.
        assert_eq!(
            account_in_passwd(passwd, 1002),
            Account::Home(b"/home/carol".to_vec())
        );
        // A home that is not an absolute path, or is empty, is damage.
        assert_eq!(account_in_passwd(passwd, 1001), Account::Damaged);
        assert_eq!(account_in_passwd(passwd, 1003), Account::Damaged);
        assert_eq!(account_in_passwd(passwd, 4242), Account::Missing);
        // The first line that names the user decides, as the system's lookup
        // does.
        let twice = b"a:x:5:5::/first:/bin/sh\nb:x:5:5::/second:/bin/sh\n";
        assert_eq!(
            account_in_passwd(twice, 5),
            Account::Home(b"/first".to_vec())
        );
        // Ids that are not plain numbers belong to nobody.
        for id in ["+1000", "-1", "1000 ", "0x3e8", "", "99999999999"] {
            let line = format!("n:x:{id}:1::/home/n:/bin/sh\n");
            assert_eq!(account_in_passwd(line.as_bytes(), 1000), Account::Missing);
        }
    }

    #[test]
    fn lines_of_other_users_may_be_any_bytes() {
        // A name in Latin-1, a comment that is not text at all, and a home
        // folder with a byte that is not UTF-8 (a Linux name may hold one).
        let mut passwd = Vec::new();
        passwd.extend_from_slice(b"ren\xe9:x:2000:2000:\xff\xfe\x00:/home/ren\xe9:/bin/sh\n");
        passwd.extend_from_slice(b"alice:x:1000:1000::/home/alice:/bin/sh\n");
        assert_eq!(
            account_in_passwd(&passwd, 1000),
            Account::Home(b"/home/alice".to_vec())
        );
        assert_eq!(
            account_in_passwd(&passwd, 2000),
            Account::Home(b"/home/ren\xe9".to_vec()),
            "a comment that is not text is no reason to refuse"
        );
        assert_eq!(
            account_in_passwd(b"x:x:7:7::/home/a\0b:/bin/sh\n", 7),
            Account::Damaged,
            "a NUL in the home folder is damage"
        );
        let clean = b"ren\xe9:x:2000:2000:Ren\xe9:/home/ren\xe9:/bin/sh\n";
        assert_eq!(
            account_in_passwd(clean, 2000),
            Account::Home(b"/home/ren\xe9".to_vec())
        );
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
        // cannot be read (here a folder stands in for it), or the user's own
        // line is damaged. Each is refused, with a code and a reason the
        // interface can show.
        f.mkdir("unreadable");
        std::fs::write(
            f.p("damaged"),
            b"alice:x:1000:1000:Alice:relative:/bin/sh\n",
        )
        .unwrap();
        for (uid, file) in [
            (4242, passwd.clone()),
            (1000, f.p("absent")),
            (1000, f.p("unreadable")),
            (1000, f.p("damaged")),
        ] {
            let judge = system_judge(uid, &file, &mountinfo);
            let (code, why) = judge.refusal().expect("deletes are refused");
            assert_eq!(code, ErrorCode::NoAccount);
            assert!(why.contains("account"), "{why}");
        }
    }

    #[test]
    fn damage_in_other_lines_does_not_refuse_the_user_with_a_good_line() {
        let f = crate::testutil::Fixture::new("other-lines");
        std::fs::write(
            f.p("passwd"),
            b"ren\xe9:x:2000:2000:\xff\xfe:/home/ren\xe9:/bin/sh\nbroken\nalice:x:1000:1000::/home/alice:/bin/sh\n",
        )
        .unwrap();
        std::fs::write(f.p("mountinfo"), b"").unwrap();
        let judge = system_judge(1000, &f.p("passwd"), &f.p("mountinfo"));
        assert_eq!(judge.refusal(), None);
    }
}
