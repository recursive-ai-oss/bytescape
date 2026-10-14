//! Putting text on the system clipboard.
//!
//! `winit` has no clipboard, and the crates that do have one pull in display
//! protocol libraries and image codecs for a job that is one line of text.
//! Instead this runs the system's own clipboard tool and writes the text to
//! its standard input: `wl-copy` on Wayland, `xclip` or `xsel` on X11 and
//! `clip` on Windows. Nothing here opens a socket or links a library.
//!
//! The tool is found on the `PATH` before anything runs, so a missing tool is
//! reported to the user at once; the copy itself runs on a thread, because
//! X11 and Wayland tools stay alive to serve the clipboard.

use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// A clipboard program and how to run it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tool {
    pub program: &'static str,
    pub args: &'static [&'static str],
    /// The tool wants UTF-16 with a byte order mark.
    pub utf16: bool,
}

const WL_COPY: Tool = Tool {
    program: "wl-copy",
    args: &[],
    utf16: false,
};
const XCLIP: Tool = Tool {
    program: "xclip",
    args: &["-selection", "clipboard", "-in"],
    utf16: false,
};
const XSEL: Tool = Tool {
    program: "xsel",
    args: &["--clipboard", "--input"],
    utf16: false,
};
const CLIP: Tool = Tool {
    program: "clip",
    args: &[],
    utf16: true,
};

/// The tools to try, best first, for a session of this kind.
pub fn candidates(windows: bool, wayland: bool) -> Vec<Tool> {
    if windows {
        vec![CLIP]
    } else if wayland {
        vec![WL_COPY, XCLIP, XSEL]
    } else {
        vec![XCLIP, XSEL, WL_COPY]
    }
}

/// The text as the tool wants it.
pub fn encode(text: &str, utf16: bool) -> Vec<u8> {
    if !utf16 {
        return text.as_bytes().to_vec();
    }
    let mut out = vec![0xFF, 0xFE];
    for unit in text.encode_utf16() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out
}

/// The first of `tools` that exists in one of the folders of `path_var`.
pub fn find(tools: &[Tool], path_var: &OsStr) -> Option<(Tool, PathBuf)> {
    for tool in tools {
        for dir in std::env::split_paths(path_var) {
            for name in names(tool.program) {
                let full = dir.join(name);
                if full.is_file() {
                    return Some((*tool, full));
                }
            }
        }
    }
    None
}

fn names(program: &str) -> Vec<String> {
    if cfg!(windows) {
        vec![format!("{program}.exe")]
    } else {
        vec![program.to_string()]
    }
}

/// Runs `program` with `tool`'s arguments, writes the text to it and waits
/// for it to exit.
pub fn run(tool: &Tool, program: &Path, text: &str) -> Result<(), String> {
    let mut child = Command::new(program)
        .args(tool.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not run {}: {e}", tool.program))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(&encode(text, tool.utf16))
            .map_err(|e| format!("could not write to {}: {e}", tool.program))?;
        // Closing the pipe tells the tool the text is complete.
    }
    let status = child
        .wait()
        .map_err(|e| format!("{} did not finish: {e}", tool.program))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{} failed", tool.program))
    }
}

/// What to tell the user when there is no clipboard tool.
pub fn missing_tool_message(windows: bool) -> &'static str {
    if windows {
        "Could not copy: the clip program is missing"
    } else {
        "Could not copy: install wl-clipboard or xclip"
    }
}

/// Copies `text` to the clipboard. Returns an error right away when no tool
/// is installed; the copy itself finishes on a background thread.
pub fn copy_text(text: &str) -> Result<(), String> {
    let windows = cfg!(windows);
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty());
    let path = std::env::var_os("PATH").unwrap_or_default();
    let Some((tool, program)) = find(&candidates(windows, wayland), &path) else {
        return Err(missing_tool_message(windows).to_string());
    };
    let text = text.to_string();
    std::thread::Builder::new()
        .name("clipboard".into())
        .spawn(move || {
            // Nobody waits for the answer; the tool either took the text or
            // the clipboard stays as it was.
            let _ = run(&tool, &program, &text);
        })
        .map(|_| ())
        .map_err(|e| format!("could not copy: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wayland_sessions_prefer_wl_copy_and_x11_sessions_xclip() {
        assert_eq!(candidates(false, true)[0].program, "wl-copy");
        assert_eq!(candidates(false, false)[0].program, "xclip");
        assert_eq!(candidates(true, false), vec![CLIP]);
        // Every Linux session has a fallback.
        assert_eq!(candidates(false, false).len(), 3);
    }

    #[test]
    fn windows_text_is_utf16_with_a_byte_order_mark() {
        assert_eq!(encode("aé", false), "aé".as_bytes());
        assert_eq!(encode("aé", true), [0xFF, 0xFE, b'a', 0, 0xE9, 0]);
        // Characters outside the basic plane become surrogate pairs.
        assert_eq!(encode("😀", true).len(), 2 + 4);
        assert_eq!(encode("", true), [0xFF, 0xFE]);
    }

    #[test]
    fn a_missing_tool_is_found_missing() {
        let nothing = std::env::temp_dir().join("bytescape-no-such-folder");
        assert!(find(&candidates(false, false), nothing.as_os_str()).is_none());
        assert!(find(&candidates(false, false), OsStr::new("")).is_none());
    }

    #[cfg(unix)]
    fn fake_tool(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(dir).expect("dir");
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("script");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("mode");
        path
    }

    #[cfg(unix)]
    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("bytescape-clip-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    #[cfg(unix)]
    #[test]
    fn the_text_reaches_the_tool_unchanged_and_the_tool_is_found_on_the_path() {
        let dir = scratch("ok");
        let out = dir.join("got.txt");
        fake_tool(
            &dir.join("bin"),
            "xclip",
            &format!("cat > '{}'", out.display()),
        );
        let path = dir.join("bin");
        let (tool, program) = find(&candidates(false, false), path.as_os_str()).expect("found");
        assert_eq!(tool.program, "xclip");
        let text = "/home/a/Video \u{202e}ünï\ncode ☃";
        run(&tool, &program, text).expect("ran");
        assert_eq!(std::fs::read(&out).expect("output"), text.as_bytes());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_failing_tool_is_an_error_not_a_silent_success() {
        let dir = scratch("fail");
        fake_tool(&dir.join("bin"), "xclip", "cat >/dev/null; exit 3");
        let path = dir.join("bin");
        let (tool, program) = find(&candidates(false, false), path.as_os_str()).expect("found");
        assert!(run(&tool, &program, "x").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_tool_that_exits_without_reading_does_not_hang_or_panic() {
        let dir = scratch("early");
        fake_tool(&dir.join("bin"), "xclip", "exit 0");
        let path = dir.join("bin");
        let (tool, program) = find(&candidates(false, false), path.as_os_str()).expect("found");
        // A big text may hit a closed pipe; either answer is fine, a hang or
        // a panic is not.
        let _ = run(&tool, &program, &"x".repeat(1 << 20));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
