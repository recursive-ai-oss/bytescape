//! Handing work to the operating system: showing an item in the file manager,
//! its properties, opening a file, a terminal, a link in the browser, the
//! system's storage tools, and an installed program's uninstaller.
//!
//! Everything here runs as the normal user. Nothing deletes anything, and
//! nothing opens a network connection: a link is given to the system's
//! opener, which decides what to start.
//!
//! # Programs started
//!
//! Each program is started with its own argument list and never through a
//! shell, so a file name cannot turn into a command. Every program is found
//! first and started by its absolute path, with standard input, output and
//! error closed; nothing else is passed on (see "Starting a program" below).
//!
//! | Program | System | Used for | Found by |
//! |---|---|---|---|
//! | `gdbus` | Linux | Show an item or its properties in the file manager; ask whether the file manager can show properties | `PATH` |
//! | `dbus-send` | Linux | The same, when `gdbus` is missing or fails | `PATH` |
//! | `xdg-open` | Linux | Open a file, a link or the parent folder; hand a software-center link to the desktop | `PATH` |
//! | `xdg-mime` | Linux | Ask whether anything handles `appstream:` links | `PATH` |
//! | `konsole`, `gnome-terminal`, `ptyxis`, `x-terminal-emulator` | Linux | Open a terminal in a folder | `PATH` |
//! | `rpm` | Linux | Ask which package owns a folder, on systems that use rpm | `PATH` |
//! | `zenity`, `kdialog` | Linux | The folder dialog | `PATH` |
//! | `wl-copy`, `xclip`, `xsel` | Linux | Put text on the clipboard | `PATH` |
//! | `wt.exe` | Windows | Open Windows Terminal in a folder | `%LOCALAPPDATA%\Microsoft\WindowsApps` |
//! | `powershell.exe` | Windows | Open a console in a folder when Windows Terminal is missing | the system folder |
//! | `cleanmgr.exe` | Windows | Disk Cleanup, when the Storage settings page cannot be opened | the system folder |
//! | the registered uninstaller, `msiexec.exe` | Windows | Uninstall a program | its registered path, which must be a local drive path; the system folder |
//!
//! On Windows, files, folders, links, terminals and uninstallers are started
//! through the shell (`ShellExecuteExW`, `SHOpenFolderAndSelectItems`) rather
//! than with `CreateProcess`, so the shell applies the user's file
//! associations and any elevation the program asks for itself. The shell never
//! reads the argument text as a command line for `cmd.exe`. Bytescape stays
//! unelevated throughout.
//!
//! # Starting a program
//!
//! - **Finding it.** On Linux the program is looked up on the user's `PATH`
//!   (the tools live in different folders on different distributions, and the
//!   process is the user's own, so `PATH` is as trusted as anything else the
//!   user runs). Only absolute `PATH` entries are searched, so a relative
//!   entry or the current folder can never supply the program, and the result
//!   must be an executable regular file. On Windows the program is looked up
//!   in a fixed system location, never the current folder or `PATH`.
//! - **Arguments.** Paths given to a program are checked first: they must be
//!   absolute, and each is one argument, so one that starts with `-` cannot be
//!   read as an option.
//! - **Handles.** On Linux standard input, output and error are closed, and
//!   every other descriptor is marked close-on-exec before the program starts.
//!   On Windows the shell starts the program without inheriting handles.
//! - **The session bus.** A D-Bus address can name a TCP transport, and a
//!   program started with one would open an internet socket on Bytescape's
//!   behalf. So a program that uses D-Bus is started only when the address is
//!   a local socket; see [`LaunchError::SessionBusNotLocal`].

use std::io;
use std::path::Path;

mod plan;
#[cfg(target_os = "linux")]
pub(crate) mod spawn;

#[cfg(target_os = "linux")]
pub(crate) mod linux;
#[cfg(windows)]
mod windows;

pub use crate::software::{Package, UninstallCommand};

#[cfg(target_os = "linux")]
pub(crate) use plan::is_component_id;

/// Why something could not be handed to the system.
#[derive(Debug)]
#[non_exhaustive]
pub enum LaunchError {
    /// The input cannot be used as given: a relative path, a path that does
    /// not exist, a link the system should not be asked to open.
    BadInput(&'static str),
    /// A program that is needed is not installed.
    ProgramMissing(&'static str),
    /// The session bus address names something other than a local socket, so
    /// nothing that uses D-Bus is started.
    SessionBusNotLocal,
    /// This system has no such action.
    Unsupported,
    /// A program or system call ran and failed.
    Failed {
        step: &'static str,
        kind: io::ErrorKind,
        /// The exit status or system error code, when there is one.
        code: Option<i32>,
    },
    /// A program did not answer in time and was stopped.
    TimedOut(&'static str),
}

impl std::fmt::Display for LaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadInput(why) => write!(f, "cannot use that: {why}"),
            Self::ProgramMissing(name) => write!(f, "{name} is not installed"),
            Self::SessionBusNotLocal => {
                f.write_str("the session bus is not a local socket, so nothing was started")
            }
            Self::Unsupported => f.write_str("not available on this system"),
            Self::Failed { step, kind, code } => {
                write!(f, "{step} failed ({kind:?}")?;
                if let Some(code) = code {
                    write!(f, ", code {code}")?;
                }
                f.write_str(")")
            }
            Self::TimedOut(step) => write!(f, "{step} did not answer in time"),
        }
    }
}

impl std::error::Error for LaunchError {}

impl LaunchError {
    #[cfg(target_os = "linux")]
    pub(crate) fn io(step: &'static str, error: &io::Error) -> Self {
        Self::Failed {
            step,
            kind: error.kind(),
            code: error.raw_os_error(),
        }
    }
}

/// Shows the item in the file manager with it selected.
///
/// Linux asks the file manager through `org.freedesktop.FileManager1`; if
/// that fails it opens the parent folder. The call can take a few seconds when
/// the file manager has to start, so call it off the interface thread.
pub fn reveal(path: &Path) -> Result<(), LaunchError> {
    #[cfg(target_os = "linux")]
    return linux::reveal(&linux::Env::current(), path);
    #[cfg(windows)]
    return windows::reveal(path);
    #[cfg(not(any(target_os = "linux", windows)))]
    return Err(unsupported(path));
}

/// Shows the system's properties window for the item.
///
/// Check [`properties_supported`] first and hide the action when it is false.
pub fn properties(path: &Path) -> Result<(), LaunchError> {
    #[cfg(target_os = "linux")]
    return linux::properties(&linux::Env::current(), path);
    #[cfg(windows)]
    return windows::properties(path);
    #[cfg(not(any(target_os = "linux", windows)))]
    return Err(unsupported(path));
}

/// Whether the system can show a properties window. On Linux it asks the file
/// manager once and remembers the answer, which can take a few seconds the
/// first time, so call it off the interface thread.
pub fn properties_supported() -> bool {
    #[cfg(target_os = "linux")]
    return linux::properties_supported();
    #[cfg(windows)]
    return true;
    #[cfg(not(any(target_os = "linux", windows)))]
    return false;
}

/// Opens a file or folder with the system's default program. The item must
/// exist.
pub fn open(path: &Path) -> Result<(), LaunchError> {
    #[cfg(target_os = "linux")]
    return linux::open(&linux::Env::current(), path);
    #[cfg(windows)]
    return windows::open(path);
    #[cfg(not(any(target_os = "linux", windows)))]
    return Err(unsupported(path));
}

/// Opens a terminal whose working folder is `dir`.
///
/// Windows uses Windows Terminal, else PowerShell. Linux picks by desktop:
/// Konsole, GNOME Terminal or Ptyxis, else `x-terminal-emulator`.
pub fn open_terminal(dir: &Path) -> Result<(), LaunchError> {
    #[cfg(target_os = "linux")]
    return linux::open_terminal(&linux::Env::current(), dir);
    #[cfg(windows)]
    return windows::open_terminal(dir);
    #[cfg(not(any(target_os = "linux", windows)))]
    return Err(unsupported(dir));
}

/// Opens a web address in the user's browser. Bytescape never connects
/// itself. Only `http:` and `https:` addresses are accepted.
pub fn open_url(url: &str) -> Result<(), LaunchError> {
    let url = plan::web_url(url)?;
    open_link(url)
}

fn open_link(url: &str) -> Result<(), LaunchError> {
    #[cfg(target_os = "linux")]
    return linux::open_link(&linux::Env::current(), url);
    #[cfg(windows)]
    return windows::open_link(url);
    #[cfg(not(any(target_os = "linux", windows)))]
    return Err(unsupported(Path::new(url)));
}

/// Whether the system has a storage tool to hand folders such as `Windows.old`
/// to: Storage settings or Disk Cleanup on Windows, nothing on Linux.
pub fn disk_cleanup_available() -> bool {
    cfg!(windows)
}

/// Opens Windows' Storage settings, or Disk Cleanup if that page cannot be
/// opened. `drive` is the letter Disk Cleanup starts on.
pub fn open_disk_cleanup(drive: Option<char>) -> Result<(), LaunchError> {
    #[cfg(windows)]
    return windows::open_disk_cleanup(drive);
    #[cfg(not(windows))]
    {
        let _ = drive;
        Err(LaunchError::Unsupported)
    }
}

/// Starts a program's registered uninstaller as the current user. The
/// uninstaller asks for administrator rights itself when it needs them.
pub fn uninstall(command: &UninstallCommand) -> Result<(), LaunchError> {
    #[cfg(windows)]
    return windows::uninstall(command);
    #[cfg(not(windows))]
    {
        let _ = command;
        Err(LaunchError::Unsupported)
    }
}

/// Whether the desktop has a program that opens software-center links
/// (Discover, GNOME Software). Linux only. It runs a short command, so call it
/// off the interface thread.
pub fn software_center_available() -> bool {
    #[cfg(target_os = "linux")]
    return linux::software_center_available(&linux::Env::current());
    #[cfg(not(target_os = "linux"))]
    return false;
}

/// Opens the package in Discover or GNOME Software, through the desktop's
/// opener. The package must have a link ([`Package::software_center_url`]).
pub fn open_in_software_center(package: &Package) -> Result<(), LaunchError> {
    let url = package
        .software_center_url
        .as_deref()
        .ok_or(LaunchError::Unsupported)?;
    let url = plan::software_center_url(url)?;
    #[cfg(target_os = "linux")]
    return linux::open_link(&linux::Env::current(), url);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = url;
        Err(LaunchError::Unsupported)
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
fn unsupported(_: &Path) -> LaunchError {
    LaunchError::Unsupported
}
