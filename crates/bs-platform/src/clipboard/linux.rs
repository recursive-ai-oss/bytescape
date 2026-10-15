//! The clipboard on Linux.

use super::ClipboardError;
use crate::launch::linux::{BusUse, Env, tool};
use crate::launch::spawn::Spec;
use std::io::Write;
use std::process::Stdio;
use std::thread;
use std::time::{Duration, Instant};

/// How long a tool may take to read the text and exit. Tools that serve the
/// clipboard from the background normally fork and exit at once; one that stays
/// is left running once it has taken all the text.
const SETTLE: Duration = Duration::from_millis(1500);

#[derive(Clone, Copy)]
struct Clip {
    program: &'static str,
    args: &'static [&'static str],
}

const WL_COPY: Clip = Clip {
    program: "wl-copy",
    args: &[],
};
const XCLIP: Clip = Clip {
    program: "xclip",
    args: &["-selection", "clipboard", "-in"],
};
const XSEL: Clip = Clip {
    program: "xsel",
    args: &["--clipboard", "--input"],
};

/// The programs to try, best first.
fn candidates(wayland: bool) -> [Clip; 3] {
    if wayland {
        [WL_COPY, XCLIP, XSEL]
    } else {
        [XCLIP, XSEL, WL_COPY]
    }
}

pub(crate) fn set_text(env: &Env, text: &str) -> Result<(), ClipboardError> {
    let mut last = ClipboardError::NoTool;
    for clip in candidates(env.wayland) {
        let Ok(spec) = tool(env, clip.program, BusUse::No) else {
            continue;
        };
        match give(&spec.args(clip.args.iter().map(Into::into)), text) {
            Ok(()) => return Ok(()),
            Err(error) => last = error,
        }
    }
    Err(last)
}

/// Starts the program, writes the text to it and waits for it to settle.
fn give(spec: &Spec, text: &str) -> Result<(), ClipboardError> {
    let mut command = spec.command();
    command.stdin(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| ClipboardError::Failed(e.kind(), e.raw_os_error()))?;
    let stdin = child.stdin.take();
    let data = text.as_bytes().to_vec();
    // A thread writes, so a program that never reads cannot hold us up. The
    // pipe closes when the thread is done, which tells the program the text
    // is complete.
    let writer = thread::spawn(move || match stdin {
        Some(mut pipe) => pipe.write_all(&data),
        None => Ok(()),
    });
    let deadline = Instant::now() + SETTLE;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                return Err(ClipboardError::Failed(
                    std::io::ErrorKind::Other,
                    status.code(),
                ));
            }
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ClipboardError::Failed(e.kind(), e.raw_os_error()));
            }
        }
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    // Still running. If it has read everything, it is serving the clipboard.
    if writer.is_finished() && writer.join().is_ok_and(|written| written.is_ok()) {
        thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    } else {
        let _ = child.kill();
        let _ = child.wait();
        Err(ClipboardError::TimedOut)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch::spawn::testing::script;
    use crate::testutil::Fixture;
    use std::ffi::OsString;

    fn env(fx: &Fixture, wayland: bool) -> Env {
        Env {
            path: fx.path().join("bin").into_os_string(),
            bus_address: Some(OsString::from("tcp:host=example.org,port=1")),
            runtime_dir: None,
            desktop: String::new(),
            socket_exists: |_| false,
            wayland,
        }
    }

    fn fixture(label: &str) -> Fixture {
        let fx = Fixture::new(label);
        fx.mkdir("bin");
        fx
    }

    fn capture(fx: &Fixture, name: &str, extra: &str) -> std::path::PathBuf {
        let out = fx.path().join(format!("{name}.txt"));
        script(
            &fx.path().join("bin"),
            name,
            &format!(
                "printf '%s\\n' \"$@\" > '{0}.args'; cat > '{0}'; {extra}",
                out.display()
            ),
        );
        out
    }

    #[test]
    fn the_text_reaches_the_tool_unchanged() {
        let fx = fixture("clip-text");
        let out = capture(&fx, "xclip", "");
        let text = "/home/a/Video \u{202e}ünï\ncode ☃ $(touch x) `y` 'z'";
        set_text(&env(&fx, false), text).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), text.as_bytes());
        assert_eq!(
            std::fs::read_to_string(format!("{}.args", out.display())).unwrap(),
            "-selection\nclipboard\n-in\n"
        );
    }

    #[test]
    fn the_session_decides_which_tool_is_tried_first() {
        let fx = fixture("clip-order");
        let wl = capture(&fx, "wl-copy", "");
        let x = capture(&fx, "xclip", "");
        set_text(&env(&fx, true), "one").unwrap();
        assert_eq!(std::fs::read_to_string(&wl).unwrap(), "one");
        assert!(!x.exists());
        set_text(&env(&fx, false), "two").unwrap();
        assert_eq!(std::fs::read_to_string(&x).unwrap(), "two");
    }

    #[test]
    fn a_tool_that_fails_hands_over_to_the_next() {
        let fx = fixture("clip-fallback");
        script(&fx.path().join("bin"), "wl-copy", "cat >/dev/null; exit 1");
        let xsel = capture(&fx, "xsel", "");
        set_text(&env(&fx, true), "text").unwrap();
        assert_eq!(std::fs::read_to_string(&xsel).unwrap(), "text");
        assert_eq!(
            std::fs::read_to_string(format!("{}.args", xsel.display())).unwrap(),
            "--clipboard\n--input\n"
        );
    }

    #[test]
    fn all_tools_failing_is_an_error_not_a_silent_success() {
        let fx = fixture("clip-fail");
        script(&fx.path().join("bin"), "xclip", "cat >/dev/null; exit 3");
        assert!(matches!(
            set_text(&env(&fx, false), "x"),
            Err(ClipboardError::Failed(_, Some(3)))
        ));
        let fx = fixture("clip-none");
        assert!(matches!(
            set_text(&env(&fx, false), "x"),
            Err(ClipboardError::NoTool)
        ));
    }

    #[test]
    fn a_tool_that_exits_without_reading_does_not_hang() {
        let fx = fixture("clip-early");
        script(&fx.path().join("bin"), "xclip", "exit 0");
        let started = Instant::now();
        // A large text may meet a closed pipe; the tool said it succeeded.
        let _ = set_text(&env(&fx, false), &"x".repeat(4 << 20));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_tool_that_stays_to_serve_the_clipboard_is_a_success() {
        let fx = fixture("clip-serve");
        let out = capture(&fx, "xclip", "sleep 4");
        let started = Instant::now();
        set_text(&env(&fx, false), "served").unwrap();
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "served");
    }

    #[test]
    fn a_tool_that_never_reads_is_stopped() {
        let fx = fixture("clip-stuck");
        script(&fx.path().join("bin"), "xclip", "exec sleep 30");
        let started = Instant::now();
        let big = "x".repeat(8 << 20);
        assert!(matches!(
            set_text(&env(&fx, false), &big),
            Err(ClipboardError::TimedOut)
        ));
        assert!(started.elapsed() < Duration::from_secs(8));
    }

    #[test]
    fn too_much_text_is_refused_before_anything_starts() {
        let big = "x".repeat(super::super::MAX_TEXT_BYTES + 1);
        assert!(matches!(
            super::super::set_text(&big),
            Err(ClipboardError::TooLarge)
        ));
    }
}
