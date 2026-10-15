//! The system's folder dialog.
//!
//! - **Windows:** the shell's file dialog in folder-picking mode
//!   (`IFileOpenDialog` with `FOS_PICKFOLDERS`).
//! - **Linux:** `zenity` or `kdialog`, chosen by the desktop. The desktop
//!   portal would be the nicer route, but its answer comes as a D-Bus signal
//!   that only a D-Bus client can wait for, and a client library would bring a
//!   socket-capable dependency into the program. Both tools run as the normal
//!   user, with an argument list and no shell; see [`crate::launch`] for how
//!   programs are started.

use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

/// Why no folder came back (as opposed to the user closing the dialog).
#[derive(Debug)]
#[non_exhaustive]
pub enum DialogError {
    /// This system has no dialog to show: neither `zenity` nor `kdialog` is
    /// installed, or the platform call failed.
    Unavailable,
    /// The session bus is not a local socket, so no dialog program is started.
    SessionBusNotLocal,
    /// The dialog answered with something that is not an existing folder.
    BadAnswer,
}

impl std::fmt::Display for DialogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unavailable => "no folder dialog is available",
            Self::SessionBusNotLocal => "the session bus is not a local socket",
            Self::BadAnswer => "the dialog did not answer with a folder",
        })
    }
}

impl std::error::Error for DialogError {}

/// Shows the folder dialog, starting in `start` when that is an existing
/// folder, and waits for it. `Ok(None)` means the user closed it without
/// choosing. The folder returned exists and is an absolute path.
///
/// Call it off the interface thread: it returns when the user is done.
pub fn pick_folder(start: &Path) -> Result<Option<PathBuf>, DialogError> {
    #[cfg(target_os = "linux")]
    return linux::pick_folder(&crate::launch::linux::Env::current(), start);
    #[cfg(windows)]
    return windows::pick_folder(start);
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = start;
        Err(DialogError::Unavailable)
    }
}
