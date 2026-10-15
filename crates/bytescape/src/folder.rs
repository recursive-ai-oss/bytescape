//! "Scan a folder…": asks the system for a folder with its own dialog.
//!
//! The dialog is the system's: `zenity` or `kdialog` on Linux, chosen by the
//! desktop, and the Windows folder browser through PowerShell. Both run as the
//! normal user and print the chosen folder. The desktop portal would be the
//! nicer route on Linux, but its answer arrives as a signal that only a
//! D-Bus client can wait for, and a client library would bring a socket-capable
//! dependency into the program; the two small tools cover the common desktops.

use std::path::PathBuf;
use std::process::{Command, Stdio};

/// One way to show the dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tool {
    pub program: &'static str,
    pub args: Vec<String>,
}

/// The tools to try, best first. `desktop` is `XDG_CURRENT_DESKTOP`.
pub fn tools(desktop: &str, start: &str) -> Vec<Tool> {
    if cfg!(windows) {
        let script = "Add-Type -AssemblyName System.Windows.Forms; \
            $d = New-Object System.Windows.Forms.FolderBrowserDialog; \
            $d.Description = 'Choose a folder to scan'; \
            if ($d.ShowDialog() -eq 'OK') { [Console]::Out.Write($d.SelectedPath) }";
        return vec![Tool {
            program: "powershell",
            args: vec!["-NoProfile".into(), "-Command".into(), script.into()],
        }];
    }
    let zenity = Tool {
        program: "zenity",
        args: vec![
            "--file-selection".into(),
            "--directory".into(),
            "--title=Choose a folder to scan".into(),
            format!("--filename={start}/"),
        ],
    };
    let kdialog = Tool {
        program: "kdialog",
        args: vec![
            "--getexistingdirectory".into(),
            start.into(),
            "--title".into(),
            "Choose a folder to scan".into(),
        ],
    };
    if desktop.to_ascii_lowercase().contains("kde") {
        vec![kdialog, zenity]
    } else {
        vec![zenity, kdialog]
    }
}

/// Whether a session bus address is one a spawned tool may use: a Unix
/// socket, or none set. A TCP address would make the tool open an IP socket.
pub fn bus_address_is_local(address: Option<&str>) -> bool {
    match address {
        None | Some("") => true,
        Some(a) => a
            .split(';')
            .all(|part| part.starts_with("unix:") || part.starts_with("systemd:")),
    }
}

/// Turns a tool's output into a folder. Only an existing directory counts.
pub fn parse_choice(output: &[u8]) -> Option<PathBuf> {
    let trimmed = output
        .strip_suffix(b"\r\n")
        .or_else(|| output.strip_suffix(b"\n"))
        .unwrap_or(output);
    if trimmed.is_empty() {
        return None;
    }
    #[cfg(unix)]
    let path = {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(trimmed))
    };
    #[cfg(not(unix))]
    let path = PathBuf::from(String::from_utf8_lossy(trimmed).into_owned());
    path.is_dir().then_some(path)
}

/// What came of asking.
#[derive(Debug, PartialEq, Eq)]
pub enum Choice {
    Folder(PathBuf),
    /// The user closed the dialog.
    Cancelled,
    /// No dialog tool could run; the text says what to do instead.
    Unavailable(String),
}

/// Shows the dialog and waits for it. Call it off the interface thread.
pub fn choose(start: &str) -> Choice {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    if !bus_address_is_local(std::env::var("DBUS_SESSION_BUS_ADDRESS").ok().as_deref()) {
        return Choice::Unavailable("The folder dialog is not available in this session".into());
    }
    for tool in tools(&desktop, start) {
        let output = Command::new(tool.program)
            .args(&tool.args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output();
        match output {
            Ok(out) if out.status.success() => {
                return match parse_choice(&out.stdout) {
                    Some(p) => Choice::Folder(p),
                    None => Choice::Cancelled,
                };
            }
            // The tool ran and the user said no.
            Ok(out) if out.status.code() == Some(1) => return Choice::Cancelled,
            // Missing or broken: try the next one.
            _ => {}
        }
    }
    Choice::Unavailable(
        "No folder dialog found. Start Bytescape with the folder as an argument".into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kde_prefers_kdialog_and_other_desktops_zenity() {
        let kde = tools("KDE", "/home/a");
        let gnome = tools("ubuntu:GNOME", "/home/a");
        if !cfg!(windows) {
            assert_eq!(kde[0].program, "kdialog");
            assert_eq!(gnome[0].program, "zenity");
            assert_eq!(kde.len(), 2);
        }
    }

    #[test]
    fn only_local_session_buses_are_used() {
        assert!(bus_address_is_local(None));
        assert!(bus_address_is_local(Some("unix:path=/run/user/1000/bus")));
        assert!(!bus_address_is_local(Some(
            "tcp:host=example.org,port=1234"
        )));
        assert!(!bus_address_is_local(Some(
            "unix:path=/x;tcp:host=h,port=1"
        )));
    }

    #[test]
    fn a_choice_must_be_an_existing_folder() {
        let dir = std::env::temp_dir();
        let mut out = dir.to_string_lossy().into_owned().into_bytes();
        out.push(b'\n');
        assert_eq!(parse_choice(&out), Some(dir));
        assert_eq!(parse_choice(b""), None);
        assert_eq!(parse_choice(b"\n"), None);
        assert_eq!(parse_choice(b"/definitely/not/here\n"), None);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_output_does_not_panic() {
        assert_eq!(parse_choice(&[b'/', 0xFF, 0xFE, b'\n']), None);
    }
}
