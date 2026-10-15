//! The decisions behind each handoff that need no system call: what an
//! address or path must look like, and the exact argument lists. Kept apart
//! so they are tested on every system.

// Some of these are used on one system only; all are tested on both.
#![allow(dead_code)]

use super::LaunchError;
use std::ffi::{OsStr, OsString};

/// The longest address handed to the system's opener.
const MAX_URL_BYTES: usize = 2048;

/// Whether the text has a character that could end an argument, a line, or
/// a command: whitespace, a control character or a NUL.
fn has_unsafe_char(text: &str) -> bool {
    text.chars().any(|c| c.is_control() || c.is_whitespace())
}

/// A web address the system may open: `http` or `https`, a host, no
/// whitespace or control characters, and a bounded length. Anything else
/// (`file:`, `javascript:`, `ms-settings:`, a custom scheme, an option such as
/// `--help`) is refused.
pub(crate) fn web_url(url: &str) -> Result<&str, LaunchError> {
    if url.len() > MAX_URL_BYTES || has_unsafe_char(url) {
        return Err(LaunchError::BadInput("the address is not usable"));
    }
    let lower = url.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))
        .ok_or(LaunchError::BadInput("only http and https addresses open"))?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    if host.is_empty() || host.starts_with('@') {
        return Err(LaunchError::BadInput("the address has no host"));
    }
    Ok(url)
}

/// A software-center link: `appstream://` and an AppStream component id made
/// of letters, digits, dots, hyphens and underscores.
pub(crate) fn software_center_url(url: &str) -> Result<&str, LaunchError> {
    let id = url
        .strip_prefix("appstream://")
        .ok_or(LaunchError::BadInput("not a software center link"))?;
    if is_component_id(id) {
        Ok(url)
    } else {
        Err(LaunchError::BadInput("not a software center link"))
    }
}

/// An AppStream component id as it appears in a metainfo file name.
pub(crate) fn is_component_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 200
        && id.starts_with(|c: char| c.is_ascii_alphanumeric())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// A `file://` address for an absolute Unix path. Everything but letters,
/// digits and `-._~/` is percent-encoded, so the result holds no quote,
/// comma, colon or space that a D-Bus tool could read as syntax.
pub(crate) fn file_uri(path: &[u8]) -> Result<String, LaunchError> {
    if path.first() != Some(&b'/') || path.contains(&0) {
        return Err(LaunchError::BadInput("the path must be absolute"));
    }
    let mut uri = String::from("file://");
    for &byte in path {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            uri.push(char::from(byte));
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    Ok(uri)
}

/// The two file-manager methods that take a list of addresses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FileManagerCall {
    ShowItems,
    ShowItemProperties,
}

impl FileManagerCall {
    fn method(self) -> &'static str {
        match self {
            Self::ShowItems => "ShowItems",
            Self::ShowItemProperties => "ShowItemProperties",
        }
    }
}

const FILE_MANAGER: &str = "org.freedesktop.FileManager1";
const FILE_MANAGER_PATH: &str = "/org/freedesktop/FileManager1";

fn os(text: impl Into<String>) -> OsString {
    OsString::from(text.into())
}

/// Arguments for `gdbus call`. `uri` comes from [`file_uri`], which cannot
/// hold a quote, so it can sit inside the text form of a string array.
pub(crate) fn gdbus_call_args(
    call: FileManagerCall,
    uri: &str,
    timeout_seconds: u32,
) -> Vec<OsString> {
    vec![
        os("call"),
        os("--session"),
        os("--timeout"),
        os(timeout_seconds.to_string()),
        os("--dest"),
        os(FILE_MANAGER),
        os("--object-path"),
        os(FILE_MANAGER_PATH),
        os("--method"),
        os(format!("{FILE_MANAGER}.{}", call.method())),
        os(format!("['{uri}']")),
        os(""),
    ]
}

/// Arguments for `dbus-send`, which separates array items with commas; the
/// encoded `uri` has none.
pub(crate) fn dbus_send_call_args(call: FileManagerCall, uri: &str) -> Vec<OsString> {
    vec![
        os("--session"),
        os("--print-reply"),
        os(format!("--dest={FILE_MANAGER}")),
        os("--type=method_call"),
        os(FILE_MANAGER_PATH),
        os(format!("{FILE_MANAGER}.{}", call.method())),
        os(format!("array:string:{uri}")),
        os("string:"),
    ]
}

/// Arguments for `gdbus introspect` on the file manager's object.
pub(crate) fn gdbus_introspect_args(timeout_seconds: u32) -> Vec<OsString> {
    vec![
        os("introspect"),
        os("--session"),
        os("--timeout"),
        os(timeout_seconds.to_string()),
        os("--dest"),
        os(FILE_MANAGER),
        os("--object-path"),
        os(FILE_MANAGER_PATH),
    ]
}

/// Arguments for `dbus-send` asking the file manager for its interfaces.
pub(crate) fn dbus_send_introspect_args() -> Vec<OsString> {
    vec![
        os("--session"),
        os("--print-reply"),
        os(format!("--dest={FILE_MANAGER}")),
        os("--type=method_call"),
        os(FILE_MANAGER_PATH),
        os("org.freedesktop.DBus.Introspectable.Introspect"),
    ]
}

/// A terminal emulator on Linux.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Emulator {
    Konsole,
    GnomeTerminal,
    Ptyxis,
    /// Debian's alternatives entry for the user's choice.
    Generic,
}

impl Emulator {
    pub(crate) fn program(self) -> &'static str {
        match self {
            Self::Konsole => "konsole",
            Self::GnomeTerminal => "gnome-terminal",
            Self::Ptyxis => "ptyxis",
            Self::Generic => "x-terminal-emulator",
        }
    }
}

/// The terminals to try, in order, for a desktop (`XDG_CURRENT_DESKTOP`, a
/// colon-separated list). The desktop's own terminal comes first, then the
/// system's generic one, then the other known ones.
pub(crate) fn terminal_order(desktop: &str) -> Vec<Emulator> {
    let names: Vec<String> = desktop
        .split(':')
        .map(|part| part.trim().to_ascii_lowercase())
        .collect();
    let has = |wanted: &str| names.iter().any(|n| n == wanted);
    let mut order = if has("kde") {
        vec![Emulator::Konsole]
    } else if has("gnome") || has("ubuntu") || has("unity") || has("pantheon") {
        vec![Emulator::Ptyxis, Emulator::GnomeTerminal]
    } else {
        Vec::new()
    };
    order.push(Emulator::Generic);
    for other in [Emulator::Konsole, Emulator::GnomeTerminal, Emulator::Ptyxis] {
        if !order.contains(&other) {
            order.push(other);
        }
    }
    order
}

/// Arguments that open `terminal` in `dir`. The folder is also the working
/// folder of the started program, so a terminal that ignores the option still
/// opens in the right place.
pub(crate) fn terminal_args(terminal: Emulator, dir: &OsStr) -> Vec<OsString> {
    match terminal {
        Emulator::Konsole => vec![os("--workdir"), dir.to_os_string()],
        Emulator::GnomeTerminal => {
            let mut option = OsString::from("--working-directory=");
            option.push(dir);
            vec![option]
        }
        Emulator::Ptyxis | Emulator::Generic => Vec::new(),
    }
}

/// A Windows path in the plain form the shell accepts. The `\\?\` prefix
/// (which the shell refuses) is removed, `/` becomes `\`, and the result must
/// be a drive path or a UNC path. Device paths, relative paths and paths with
/// a NUL are refused.
pub(crate) fn plain_windows_path(path: &str) -> Result<String, LaunchError> {
    let bad = || LaunchError::BadInput("the path must be an absolute Windows path");
    let text = if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = path.strip_prefix(r"\\?\") {
        rest.to_owned()
    } else {
        path.to_owned()
    };
    let text = text.replace('/', "\\");
    if text.contains('\0') || text.starts_with(r"\\?\") || text.starts_with(r"\\.\") {
        return Err(bad());
    }
    let bytes = text.as_bytes();
    let drive =
        bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\';
    let unc = text.strip_prefix(r"\\").is_some_and(|rest| {
        let mut parts = rest.split('\\');
        parts.next().is_some_and(|host| !host.is_empty())
            && parts.next().is_some_and(|share| !share.is_empty())
    });
    if drive || unc { Ok(text) } else { Err(bad()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn only_web_addresses_open_in_the_browser() {
        for good in [
            "https://recursiveai.org/",
            "HTTPS://RecursiveAI.org/?a=b&c=d#top",
            "http://localhost:8080",
            "https://github.com/recursive-ai-oss/bytescape/issues/new?title=x%20y&body=z",
        ] {
            assert_eq!(web_url(good).unwrap(), good);
        }
        for bad in [
            "",
            "--help",
            "-o /tmp/x",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "ms-settings:storagesense",
            "appstream://org.example.App",
            "mailto:a@b.c",
            "https://",
            "https:///path",
            "https://@evil",
            "https://a.org/ with space",
            "https://a.org/\nsecond line",
            "https://a.org/\u{7f}",
            "https://a.org/\0",
            "ftp://a.org/",
            " https://a.org/",
        ] {
            assert!(web_url(bad).is_err(), "{bad:?}");
        }
        let long = format!("https://a.org/{}", "x".repeat(MAX_URL_BYTES));
        assert!(web_url(&long).is_err());
    }

    #[test]
    fn software_center_links_carry_a_plain_component_id() {
        assert!(software_center_url("appstream://org.mozilla.firefox").is_ok());
        assert!(software_center_url("appstream://org.kde.kate_1-x").is_ok());
        for bad in [
            "appstream://",
            "appstream://-flag",
            "appstream://a b",
            "appstream://a/b",
            "appstream://a;b",
            "appstream://a?b",
            "https://example.org/",
            "snap://x",
        ] {
            assert!(software_center_url(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_file_address_encodes_everything_that_is_not_plain() {
        assert_eq!(file_uri(b"/home/a/b.txt").unwrap(), "file:///home/a/b.txt");
        assert_eq!(
            file_uri(b"/tmp/it's a 'quote', [x]:y").unwrap(),
            "file:///tmp/it%27s%20a%20%27quote%27%2C%20%5Bx%5D%3Ay"
        );
        assert_eq!(
            file_uri("/tmp/é☃".as_bytes()).unwrap(),
            "file:///tmp/%C3%A9%E2%98%83"
        );
        // Bytes that are not text still encode.
        assert_eq!(file_uri(b"/tmp/\xff\xfe").unwrap(), "file:///tmp/%FF%FE");
        assert_eq!(file_uri(b"/a\nb").unwrap(), "file:///a%0Ab");
        assert!(file_uri(b"relative/path").is_err());
        assert!(file_uri(b"").is_err());
        assert!(file_uri(b"-flag").is_err());
        assert!(file_uri(b"/a\0b").is_err());
    }

    #[test]
    fn an_encoded_address_never_holds_syntax_for_the_tools() {
        let nasty: Vec<u8> = (1u8..=255).filter(|b| *b != b'/').collect();
        let mut path = b"/".to_vec();
        path.extend(nasty);
        let uri = file_uri(&path).unwrap();
        let body = uri.strip_prefix("file://").unwrap();
        for forbidden in [
            '\'', '"', ',', ':', ' ', '\\', '[', ']', ';', '$', '`', '\n',
        ] {
            assert!(!body.contains(forbidden), "{forbidden:?}");
        }
    }

    #[test]
    fn the_gdbus_call_is_one_argument_per_part() {
        let args = gdbus_call_args(FileManagerCall::ShowItems, "file:///a%20b", 5);
        assert_eq!(
            text(&args),
            [
                "call",
                "--session",
                "--timeout",
                "5",
                "--dest",
                "org.freedesktop.FileManager1",
                "--object-path",
                "/org/freedesktop/FileManager1",
                "--method",
                "org.freedesktop.FileManager1.ShowItems",
                "['file:///a%20b']",
                ""
            ]
        );
        let props = gdbus_call_args(FileManagerCall::ShowItemProperties, "file:///x", 5);
        assert_eq!(
            text(&props)[9],
            "org.freedesktop.FileManager1.ShowItemProperties"
        );
    }

    #[test]
    fn the_dbus_send_call_is_one_argument_per_part() {
        let args = dbus_send_call_args(FileManagerCall::ShowItems, "file:///a%2Cb");
        assert_eq!(
            text(&args),
            [
                "--session",
                "--print-reply",
                "--dest=org.freedesktop.FileManager1",
                "--type=method_call",
                "/org/freedesktop/FileManager1",
                "org.freedesktop.FileManager1.ShowItems",
                "array:string:file:///a%2Cb",
                "string:"
            ]
        );
    }

    #[test]
    fn terminals_are_chosen_by_desktop() {
        use Emulator::*;
        assert_eq!(terminal_order("KDE")[0], Konsole);
        assert_eq!(terminal_order("ubuntu:GNOME")[..2], [Ptyxis, GnomeTerminal]);
        assert_eq!(terminal_order("GNOME")[0], Ptyxis);
        // Other desktops use the system's generic terminal first.
        assert_eq!(terminal_order("XFCE")[0], Generic);
        assert_eq!(terminal_order("")[0], Generic);
        // Every known terminal is tried exactly once, whatever the desktop.
        for desktop in ["KDE", "GNOME", "XFCE", "", "weird:::"] {
            let order = terminal_order(desktop);
            assert_eq!(order.len(), 4, "{desktop}");
            for terminal in [Konsole, GnomeTerminal, Ptyxis, Generic] {
                assert_eq!(order.iter().filter(|t| **t == terminal).count(), 1);
            }
        }
    }

    #[test]
    fn a_folder_is_one_argument_to_the_terminal() {
        let dir = "/home/a/My Folder; rm -rf ~";
        assert_eq!(
            text(&terminal_args(Emulator::Konsole, OsStr::new(dir))),
            ["--workdir", dir]
        );
        assert_eq!(
            text(&terminal_args(Emulator::GnomeTerminal, OsStr::new(dir))),
            [format!("--working-directory={dir}")]
        );
        assert!(terminal_args(Emulator::Ptyxis, OsStr::new(dir)).is_empty());
        assert!(terminal_args(Emulator::Generic, OsStr::new(dir)).is_empty());
    }

    #[test]
    fn windows_paths_lose_the_verbatim_prefix() {
        assert_eq!(plain_windows_path(r"C:\Users\a").unwrap(), r"C:\Users\a");
        assert_eq!(
            plain_windows_path(r"\\?\C:\Users\a").unwrap(),
            r"C:\Users\a"
        );
        assert_eq!(plain_windows_path("C:/Users/a").unwrap(), r"C:\Users\a");
        assert_eq!(
            plain_windows_path(r"\\?\UNC\host\share\f").unwrap(),
            r"\\host\share\f"
        );
        assert_eq!(
            plain_windows_path(r"\\host\share").unwrap(),
            r"\\host\share"
        );
        for bad in [
            "",
            "relative",
            r"C:relative",
            r"\Users",
            r"\\.\PhysicalDrive0",
            r"\\?\Volume{1234}\x",
            r"\\?\\\bad",
            "C:\\a\0b",
            r"\\\x",
            "-flag",
        ] {
            assert!(plain_windows_path(bad).is_err(), "{bad:?}");
        }
    }
}
