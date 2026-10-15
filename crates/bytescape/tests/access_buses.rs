//! The Linux screen-reader adapter is made only when both buses it would use
//! are local sockets. These tests start the real program with no display, so
//! it stops at window creation, and read what it says first.

#![cfg(target_os = "linux")]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};

const OFF: &str = "screen-reader support is off";

fn run(session: &str, at_spi: Option<&str>, path: &Path) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bytescape"));
    command
        .arg("--bench-first-frame")
        .env_clear()
        .env("PATH", path)
        .env("DBUS_SESSION_BUS_ADDRESS", session)
        .env("XDG_RUNTIME_DIR", path)
        .stdin(Stdio::null());
    if let Some(at_spi) = at_spi {
        command.env("AT_SPI_BUS_ADDRESS", at_spi);
    }
    command.output().expect("the program starts")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// When this test run is itself elevated (a root container, a user namespace
/// mapped to root), the program refuses to start before it looks at any bus.
/// Returns true after checking that the refusal, and only it, was said.
fn refused_as_elevated(text: &str) -> bool {
    match bs_platform::elevation::refusal() {
        Some(message) => {
            assert_eq!(text.matches(message).count(), 1, "{text}");
            assert!(!text.contains(OFF), "{text}");
            true
        }
        None => false,
    }
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("bytescape-access-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn a_tcp_accessibility_bus_is_refused_with_one_line() {
    let dir = scratch("tcp");
    for address in [
        "tcp:host=example.org,port=4000",
        "nonce-tcp:host=h,port=1,noncefile=/x",
        "unixexec:path=/bin/sh",
        "autolaunch:",
        "launchd:env=X",
        "unix:path=/run/user/1/at-spi/bus_0;tcp:host=h,port=1",
        "tcp:host=h,port=1;unix:path=/run/user/1/at-spi/bus_0",
        "garbage",
    ] {
        let out = run("unix:path=/run/user/1/bus", Some(address), &dir);
        let text = stderr(&out);
        if refused_as_elevated(&text) {
            continue;
        }
        assert_eq!(text.matches(OFF).count(), 1, "{address}: {text}");
        assert!(
            text.contains("accessibility bus address"),
            "{address}: {text}"
        );
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_tcp_session_bus_is_refused_with_one_line() {
    let dir = scratch("session");
    for session in [
        "tcp:host=example.org,port=4000",
        "unix:path=/a;tcp:host=h,port=1",
        "unixexec:path=/bin/sh",
        "",
    ] {
        let out = run(session, Some("unix:path=/run/user/1/at-spi/bus_0"), &dir);
        let text = stderr(&out);
        if refused_as_elevated(&text) {
            continue;
        }
        assert_eq!(text.matches(OFF).count(), 1, "{session}: {text}");
        assert!(text.contains("session bus address"), "{session}: {text}");
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn no_way_to_ask_for_the_accessibility_bus_means_no_adapter() {
    let dir = scratch("nogdbus");
    let out = run("unix:path=/run/user/1/bus", None, &dir);
    let text = stderr(&out);
    if refused_as_elevated(&text) {
        return;
    }
    assert_eq!(text.matches(OFF).count(), 1, "{text}");
    assert!(text.contains("no accessibility bus is available"), "{text}");
    let _ = std::fs::remove_dir_all(dir);
}

fn fake_gdbus(dir: &Path, reply: &str) {
    let script = dir.join("gdbus");
    std::fs::write(&script, format!("#!/bin/sh\necho \"{reply}\"\n")).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn a_tcp_reply_from_the_session_bus_is_refused() {
    let dir = scratch("reply");
    fake_gdbus(&dir, "('tcp:host=example.org,port=4000',)");
    // The shell script needs `sh` and `echo`, found through the same path.
    let path = format!("{}:/usr/bin:/bin", dir.display());
    let out = run("unix:path=/run/user/1/bus", None, Path::new(&path));
    let text = stderr(&out);
    if refused_as_elevated(&text) {
        return;
    }
    assert_eq!(text.matches(OFF).count(), 1, "{text}");
    assert!(text.contains("accessibility bus address"), "{text}");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn local_buses_are_accepted_and_nothing_is_said() {
    let dir = scratch("ok");
    fake_gdbus(&dir, "('unix:path=/run/user/1/at-spi/bus_0',)");
    let path = format!("{}:/usr/bin:/bin", dir.display());
    for at_spi in [None, Some("unix:path=/run/user/1/at-spi/bus_0")] {
        let out = run("unix:path=/run/user/1/bus", at_spi, Path::new(&path));
        let text = stderr(&out);
        if refused_as_elevated(&text) {
            continue;
        }
        assert!(!text.contains(OFF), "{at_spi:?}: {text}");
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn measurement_runs_can_leave_the_adapter_out_without_a_message() {
    let dir = scratch("skip");
    let out = Command::new(env!("CARGO_BIN_EXE_bytescape"))
        .args(["--bench-first-frame", "--bench-no-access"])
        .env_clear()
        .env("PATH", &dir)
        .env("DBUS_SESSION_BUS_ADDRESS", "tcp:host=h,port=1")
        .output()
        .unwrap();
    assert!(!stderr(&out).contains(OFF));
    let _ = std::fs::remove_dir_all(dir);
}
