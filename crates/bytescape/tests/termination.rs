//! A program ended from outside (a signal, a closed display) is not a crash:
//! it leaves no report, so the next start does not say that Bytescape closed
//! unexpectedly. A panic still does; the platform crate's tests cover that.
//!
//! This needs `xvfb-run`. Without it the test prints `NOT RUN`;
//! `BYTESCAPE_REQUIRE_DISPLAY=1` makes that a failure.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_bytescape");

fn has_xvfb() -> bool {
    Command::new("xvfb-run")
        .arg("--help")
        .output()
        .is_ok_and(|o| o.status.success() || !o.stdout.is_empty() || !o.stderr.is_empty())
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch folder");
    dir
}

/// Every file the crash store holds under `state`.
fn crash_files(state: &Path) -> Vec<PathBuf> {
    let dir = state.join("bytescape").join("crashes");
    match std::fs::read_dir(dir) {
        Ok(rd) => rd.flatten().map(|e| e.path()).collect(),
        Err(_) => Vec::new(),
    }
}

/// Starts the program in a virtual display, lets it open its window, ends it
/// the way `how` says, and returns the shell's output.
fn end_it(name: &str, how: &str) -> Option<(std::process::Output, PathBuf)> {
    if bs_platform::elevation::refusal().is_some() {
        println!("NOT RUN: the program refuses to run elevated");
        return None;
    }
    if !has_xvfb() {
        assert!(
            std::env::var_os("BYTESCAPE_REQUIRE_DISPLAY").is_none(),
            "xvfb-run is required"
        );
        println!("NOT RUN: no xvfb-run");
        return None;
    }
    let dir = scratch(name);
    let state = dir.join("state");
    let folder = dir.join("folder");
    std::fs::create_dir_all(&state).expect("state folder");
    std::fs::create_dir_all(&folder).expect("folder");
    std::fs::write(folder.join("a.bin"), vec![0u8; 4096]).expect("a file");
    // The program runs in the background of a small script that waits until
    // the window has been up for a moment, then ends it.
    let script = format!(
        r#""$BS_BIN" "$BS_DIR" >/dev/null 2>&1 &
pid=$!
sleep 4
{how}
sleep 2
if kill -0 $pid 2>/dev/null; then echo survived; kill -KILL $pid; fi
wait $pid
echo "status=$?"
"#
    );
    let out = Command::new("xvfb-run")
        .args(["-a", "-s", "-screen 0 1280x800x24", "sh", "-c", &script])
        .env_remove("WAYLAND_DISPLAY")
        .env("BS_BIN", BIN)
        .env("BS_DIR", &folder)
        .env("XDG_STATE_HOME", &state)
        .output()
        .expect("xvfb-run starts");
    Some((out, state))
}

/// Whether this process was started with SIGHUP ignored (under `nohup`),
/// which every program it starts then inherits.
fn hangup_is_ignored() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|text| {
            text.lines()
                .find_map(|l| l.strip_prefix("SigIgn:"))
                .and_then(|hex| u64::from_str_radix(hex.trim(), 16).ok())
        })
        .is_some_and(|mask| mask & 1 != 0)
}

fn status_of(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .rev()
        .find(|l| l.starts_with("status="))
        .unwrap_or("status=(none)")
        .to_string()
}

#[test]
fn ending_by_signal_leaves_no_crash_report() {
    // Ctrl+C in a terminal is SIGINT; it takes the same road as SIGTERM, but a
    // background job of a script ignores it, so it cannot be sent from here.
    for (name, signal, expect) in [("term", "TERM", "status=143"), ("hup", "HUP", "status=129")] {
        if signal == "HUP" && hangup_is_ignored() {
            // Started under `nohup`, so the program inherits "ignore".
            println!("NOT RUN: SIGHUP is ignored by this test run");
            continue;
        }
        let how = format!("kill -{signal} $pid");
        let Some((out, state)) = end_it(&format!("signal-{name}"), &how) else {
            return;
        };
        let said = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(!said.contains("survived"), "{signal}: {said}");
        assert_eq!(status_of(&out), expect, "{signal}: {out:?}");
        assert_eq!(crash_files(&state), Vec::<PathBuf>::new(), "{signal}");
    }
}

#[test]
fn the_display_going_away_leaves_no_crash_report() {
    // `xvfb-run` exports the display it chose; ending its server breaks the
    // program's connection.
    let how = r#"pkill -u "$(id -u)" -f "Xvfb $DISPLAY " ; sleep 1"#;
    let Some((out, state)) = end_it("display-gone", how) else {
        return;
    };
    assert_eq!(crash_files(&state), Vec::<PathBuf>::new(), "{out:?}");
}
