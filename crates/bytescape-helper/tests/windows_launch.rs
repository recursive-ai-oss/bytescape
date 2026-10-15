//! The Windows launch path: the pipe, the checks on who is at each end, and
//! the whole round trip through the shell's elevation verb.
//!
//! Nothing here has run on a Windows machine when this was written; these
//! tests are what the Windows runner in CI is for. The round trip needs a
//! machine that may be changed (`BYTESCAPE_DISPOSABLE_HOST=1`) and an account
//! that can elevate without being asked, as a CI runner can: elsewhere it
//! prints `NOT RUN` and passes.
//!
//! The checks on the pipe start the helper directly with `CreateProcess`.
//! The helper's manifest asks for administrator rights, so a program that
//! starts it this way needs an elevated token itself (Windows refuses with
//! error 740 otherwise). A CI runner has one; from your own machine, start the
//! tests from an elevated prompt. Without it they stop at once and say so,
//! instead of failing on the launch.

#![cfg(windows)]
#![allow(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::disallowed_types, clippy::disallowed_methods)]

use bs_fsops::winpipe::PipeEnd;
use bs_platform::elevate::{DeleteTarget, Elevation};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED,
};
use windows_sys::Win32::System::Pipes::{
    CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};

const NONCE_HEX: &str = "01234567890123450123456789012345";
static COUNTER: AtomicU32 = AtomicU32::new(0);

fn helper() -> &'static str {
    env!("CARGO_BIN_EXE_bytescape-helper")
}

fn disposable() -> bool {
    std::env::var("BYTESCAPE_DISPOSABLE_HOST").as_deref() == Ok("1")
}

/// A folder made for one test and removed afterwards; nothing is deleted
/// outside it.
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let base = fs::canonicalize(std::env::temp_dir()).unwrap();
        let root = base.join(format!(
            "bs-helper-win-{label}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        Self {
            root: fs::canonicalize(root).unwrap(),
        }
    }

    fn p(&self, relative: &str) -> PathBuf {
        assert!(!relative.contains(".."), "{relative}");
        self.root.join(relative)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let named = self
            .root
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("bs-helper-win-"));
        if named {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

fn unique_pipe_name() -> String {
    // 32 digits: the name must be 128 bits of lower-case hex to be accepted.
    let n =
        u128::from(std::process::id()) << 64 | u128::from(COUNTER.fetch_add(1, Ordering::Relaxed));
    format!(r"\\.\pipe\bytescape-{n:032x}")
}

/// A pipe like the interface's, but with the default access rules.
fn server(name: &str) -> PipeEnd {
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` is NUL-terminated; the security attributes are null.
    let handle = unsafe {
        CreateNamedPipeW(
            wide.as_ptr(),
            0x3 | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            4096,
            4096,
            0,
            std::ptr::null(),
        )
    };
    assert_ne!(
        handle,
        INVALID_HANDLE_VALUE,
        "{}",
        std::io::Error::last_os_error()
    );
    // SAFETY: a new handle opened for overlapped I/O, owned here.
    unsafe { PipeEnd::from_raw(handle) }
}

/// Stops the test with a clear message unless this process has an
/// administrator token (`net session` needs one).
fn require_administrator() {
    let admin = Command::new("net")
        .arg("session")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    assert!(
        admin,
        "these tests start bytescape-helper.exe, whose manifest requires administrator rights: run them from an elevated prompt"
    );
}

fn run_helper(args: &[&str]) -> std::process::ExitStatus {
    require_administrator();
    Command::new(helper())
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
}

#[test]
fn a_pipe_name_outside_the_schema_ends_the_helper_before_it_connects() {
    for name in [
        r"\\.\pipe\other",
        r"\\.\pipe\bytescape-short",
        r"\\.\pipe\..\evil",
        r"C:\Windows\System32\drivers\etc\hosts",
    ] {
        let status = run_helper(&[
            "delete",
            "--protocol",
            "1",
            "--nonce",
            NONCE_HEX,
            "--pipe",
            name,
            "--parent",
            "4",
        ]);
        assert_eq!(status.code(), Some(2), "{name}");
    }
}

#[test]
fn a_server_that_is_not_the_process_named_on_the_command_line_is_refused() {
    let name = unique_pipe_name();
    let mut pipe = server(&name);
    // Process 4 (the system) did not create this pipe.
    let status = run_helper(&[
        "delete",
        "--protocol",
        "1",
        "--nonce",
        NONCE_HEX,
        "--pipe",
        &name,
        "--parent",
        "4",
    ]);
    assert_eq!(status.code(), Some(3));
    // Nothing was said to the stranger.
    let mut byte = [0u8; 1];
    assert_eq!(std::io::Read::read(&mut pipe, &mut byte).unwrap(), 0);
}

#[test]
fn a_server_that_is_not_the_interface_program_is_refused() {
    // The right process id, but this test program is not `bytescape.exe`
    // beside the helper.
    let name = unique_pipe_name();
    let mut pipe = server(&name);
    let me = std::process::id().to_string();
    let status = run_helper(&[
        "delete",
        "--protocol",
        "1",
        "--nonce",
        NONCE_HEX,
        "--pipe",
        &name,
        "--parent",
        &me,
    ]);
    assert_eq!(status.code(), Some(3));
    let mut byte = [0u8; 1];
    assert_eq!(std::io::Read::read(&mut pipe, &mut byte).unwrap(), 0);
}

#[test]
fn a_helper_with_no_pipe_to_connect_to_gives_up() {
    let name = unique_pipe_name();
    let me = std::process::id().to_string();
    let status = run_helper(&[
        "delete",
        "--protocol",
        "1",
        "--nonce",
        NONCE_HEX,
        "--pipe",
        &name,
        "--parent",
        &me,
    ]);
    assert_eq!(status.code(), Some(3));
}

fn target(path: &Path) -> DeleteTarget {
    DeleteTarget {
        path: path.to_path_buf(),
        confirmed_protected: false,
        seen: None,
    }
}

/// Runs inside a copy of this program that is named like the interface. It
/// does nothing in a normal run.
#[test]
fn stand_in_for_the_interface() {
    let Some(file) = std::env::var_os("BS_STAND_IN_DELETE") else {
        return;
    };
    let helper = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .join("bytescape-helper.exe");
    let result = Elevation::with_helper(helper).delete(&[target(Path::new(&file))]);
    if std::env::var_os("BS_STAND_IN_EXPECT_REFUSAL").is_some() {
        assert!(
            result.is_err(),
            "the helper must refuse this program: {result:?}"
        );
        assert!(Path::new(&file).exists());
    } else {
        let outcomes = result.unwrap();
        assert!(
            matches!(outcomes[0], bs_ipc::ItemOutcome::Deleted { .. }),
            "{outcomes:?}"
        );
        assert!(!Path::new(&file).exists());
    }
}

fn run_stand_in(fx: &Fixture, program_name: &str, victim: &Path, expect_refusal: bool) {
    let app = fx.p("app");
    fs::create_dir_all(&app).unwrap();
    fs::copy(helper(), app.join("bytescape-helper.exe")).unwrap();
    let copy = app.join(program_name);
    fs::copy(std::env::current_exe().unwrap(), &copy).unwrap();
    let mut command = Command::new(&copy);
    command
        .args([
            "--exact",
            "stand_in_for_the_interface",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("BS_STAND_IN_DELETE", victim);
    if expect_refusal {
        command.env("BS_STAND_IN_EXPECT_REFUSAL", "1");
    }
    let status = command.status().unwrap();
    assert!(status.success(), "the stand-in failed: {status}");
}

#[test]
fn the_whole_round_trip_through_the_shells_elevation_verb() {
    if !disposable() {
        println!("NOT RUN: BYTESCAPE_DISPOSABLE_HOST=1 is not set");
        return;
    }
    let fx = Fixture::new("round-trip");
    let victim = fx.p("work/victim.txt");
    fs::create_dir_all(victim.parent().unwrap()).unwrap();
    fs::write(&victim, "x").unwrap();
    run_stand_in(&fx, "bytescape.exe", &victim, false);
    assert!(!victim.exists());
}

#[test]
fn a_program_that_is_not_named_like_the_interface_is_not_served() {
    if !disposable() {
        println!("NOT RUN: BYTESCAPE_DISPOSABLE_HOST=1 is not set");
        return;
    }
    let fx = Fixture::new("impostor");
    let victim = fx.p("work/victim.txt");
    fs::create_dir_all(victim.parent().unwrap()).unwrap();
    fs::write(&victim, "x").unwrap();
    run_stand_in(&fx, "impostor.exe", &victim, true);
    assert!(victim.exists());
}
