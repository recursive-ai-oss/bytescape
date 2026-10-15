//! Putting text on the system clipboard.
//!
//! - **Windows:** `OpenClipboard` and `SetClipboardData` with Unicode text.
//! - **Linux:** the session's own clipboard program (`wl-copy` on Wayland,
//!   `xclip` or `xsel` on X11) is given the text on its standard input. A
//!   clipboard crate would pull display-protocol libraries and image codecs
//!   into the program for one line of text. See [`crate::launch`] for how
//!   programs are started.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

/// The most text copied at once. A path or a command is a few hundred bytes.
pub const MAX_TEXT_BYTES: usize = 16 * 1024 * 1024;

/// Why the text is not on the clipboard.
#[derive(Debug)]
#[non_exhaustive]
pub enum ClipboardError {
    /// No clipboard program is installed (Linux).
    NoTool,
    /// The text is longer than [`MAX_TEXT_BYTES`].
    TooLarge,
    /// The text has a NUL, which the Windows clipboard cannot hold.
    ContainsNul,
    /// The system refused: another program holds the clipboard, or the
    /// program that was started failed.
    Failed(std::io::ErrorKind, Option<i32>),
    /// The program did not finish in time and was stopped.
    TimedOut,
}

impl std::fmt::Display for ClipboardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoTool => f.write_str("no clipboard program is installed"),
            Self::TooLarge => f.write_str("the text is too long to copy"),
            Self::ContainsNul => f.write_str("the text contains a NUL character"),
            Self::Failed(kind, Some(code)) => write!(f, "copying failed ({kind:?}, code {code})"),
            Self::Failed(kind, None) => write!(f, "copying failed ({kind:?})"),
            Self::TimedOut => f.write_str("the clipboard did not answer in time"),
        }
    }
}

impl std::error::Error for ClipboardError {}

/// Replaces the clipboard's contents with `text`.
///
/// Returns once the system has taken the text. It can wait a second or two for
/// a busy clipboard, so call it off the interface thread.
pub fn set_text(text: &str) -> Result<(), ClipboardError> {
    if text.len() > MAX_TEXT_BYTES {
        return Err(ClipboardError::TooLarge);
    }
    #[cfg(target_os = "linux")]
    return linux::set_text(&crate::launch::linux::Env::current(), text);
    #[cfg(windows)]
    return windows::set_text(text);
    #[cfg(not(any(target_os = "linux", windows)))]
    Err(ClipboardError::NoTool)
}
